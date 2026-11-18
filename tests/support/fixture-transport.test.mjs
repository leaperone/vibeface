import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { test } from 'node:test';
import { createFixtureTransport, FixtureOperationError } from './fixture-transport.ts';

// IPC keeps tests in control of response order independently of fixture stdin/stdout.
const fakeFixture = String.raw`
const fs = require('node:fs');
const input = require('node:readline').createInterface({ input: process.stdin });
const requests = [];
const reply = value => process.stdout.write(JSON.stringify(value) + '\n');
let waitForSignal = false;
input.on('line', line => {
  const envelope = JSON.parse(line);
  requests.push(envelope);
  process.send(envelope);
});
input.on('close', () => { if (!waitForSignal) process.disconnect(); });
process.on('message', ({ op, value }) => {
  if (op === 'reply') reply(value);
  if (op === 'raw') process.stdout.write(value);
  if (op === 'end') fs.closeSync(1);
  if (op === 'requests') process.send(requests);
});
`;

function fixture(t, setup = 'reply({ ready: { fixture: true } });', options = {}) {
  const child = spawn(process.execPath, ['--eval', fakeFixture + setup], { stdio: ['pipe', 'pipe', 'inherit', 'ipc'] });
  const transport = createFixtureTransport(child, {
    requestTimeoutMs: 4000,
    shutdownTimeoutsMs: [1000, 1000, 1000],
    ...options,
  });
  let status;
  const closed = new Promise(resolve => child.once('close', (code, signal) => {
    status = { code, signal };
    resolve(status);
  }));
  t.after(async () => {
    if (!status) child.kill('SIGKILL');
    await closed;
  });
  return { child, transport, closed };
}

async function failure(result, pattern) {
  return result.then(() => assert.fail('Expected fixture failure'), error => {
    assert.match(error.message, pattern);
    return error;
  });
}

const inspect = { op: 'inspect' };
const nextMessage = child => once(child, 'message').then(([message]) => message);

test('startup has its own envelope and matched operation errors keep the connection usable', async t => {
  const { transport, child } = fixture(t);
  assert.deepEqual(await transport.ready, { fixture: true });
  const circular = {};
  circular.loop = circular;
  await assert.rejects(transport.call(circular), /circular/);

  const first = nextMessage(child);
  const rejected = assert.rejects(transport.call(inspect), error =>
    error instanceof FixtureOperationError && error.message === 'Fixture AssertionError at verify-node.py:123:e2e_fixture');
  await assert.rejects(transport.call(inspect), /unavailable or busy/);
  assert.deepEqual(await first, { id: 1, request: inspect });
  child.send({ op: 'reply', value: {
    id: 1, ok: false, error: 'AssertionError', frames: ['/private/file:45:secret', 'verify-node.py:123:e2e_fixture'],
  } });
  await rejected;

  const second = nextMessage(child);
  const result = transport.call(inspect);
  assert.deepEqual(await second, { id: 2, request: inspect });
  child.send({ op: 'reply', value: { id: 2, ok: true, result: { completed: true } } });
  assert.deepEqual(await result, { completed: true });
  assert.deepEqual(await transport.stop(), { code: 0, signal: null });
});

test('timeout fences late responses and all later operations before writing', async t => {
  const { transport, child } = fixture(t, undefined, { requestTimeoutMs: 500 });
  await transport.ready;
  const first = nextMessage(child);
  const timedOut = failure(transport.call(inspect), /timed out.*outcome is unknown/);
  assert.deepEqual(await first, { id: 1, request: inspect });
  const reason = await timedOut;
  await assert.rejects(transport.call(inspect), error => error === reason);
  const late = once(child.stdout, 'data');
  child.send({ op: 'reply', value: { id: 1, ok: true, result: { late: true } } });
  await late;
  await assert.rejects(transport.call(inspect), error => error === reason);
  const written = nextMessage(child);
  child.send({ op: 'requests' });
  assert.deepEqual(await written, [{ id: 1, request: inspect }]);
  await transport.stop();
});

for (const [name, wrongId] of [['unknown', 19], ['duplicate', 1]]) {
  test(`a response with a ${name === 'unknown' ? 'previously unused' : 'repeated'} ID cannot settle a different operation`, async t => {
    const { transport, child } = fixture(t);
    await transport.ready;
    const first = nextMessage(child);
    const finished = transport.call(inspect);
    await first;
    const result = { id: 1, ok: true, result: {} };
    child.send({ op: 'reply', value: result });
    assert.deepEqual(await finished, {});

    const second = nextMessage(child);
    const rejected = failure(transport.call(inspect), /ID does not match.*outcome is unknown/);
    assert.deepEqual(await second, { id: 2, request: inspect });
    child.send({ op: 'reply', value: { ...result, id: wrongId } });
    const reason = await rejected;
    await assert.rejects(transport.call(inspect), error => error === reason);
    await transport.stop();
  });
}

test('repeated readiness while an operation is pending terminates the connection', async t => {
  const { transport, child } = fixture(t);
  await transport.ready;
  const first = nextMessage(child);
  const rejected = failure(transport.call(inspect), /invalid response.*outcome is unknown/);
  await first;
  child.send({ op: 'reply', value: { ready: {} } });
  await rejected;
  await transport.stop();
});

test('startup failure has sanitized diagnostics and an ordinary response cannot signal ready', async t => {
  const startup = fixture(t, `reply({ startup_error: { error: 'AssertionError', frames: ['verify-node.py:123:e2e_fixture'] } });`);
  const reason = await failure(startup.transport.ready, /startup failure AssertionError at verify-node\.py/);
  assert(!(reason instanceof FixtureOperationError));
  await assert.rejects(startup.transport.call(inspect), error => error === reason);
  await startup.transport.stop();

  const wrongEnvelope = fixture(t, 'reply({ id: 1, ok: true, result: {} });');
  await assert.rejects(wrongEnvelope.transport.ready, /unexpected or invalid response/);
  await wrongEnvelope.transport.stop();
});

test('spawn failure settles readiness and cleanup waits for close', async () => {
  const child = spawn(process.execPath + '-missing-fixture', [], { stdio: ['pipe', 'pipe', 'ignore'] });
  const close = new Promise(resolve => child.once('close', resolve));
  const transport = createFixtureTransport(child, { requestTimeoutMs: 4000, shutdownTimeoutsMs: [1000, 1000, 1000] });
  const reason = await failure(transport.ready, /child process failed/);
  await assert.rejects(transport.call(inspect), error => error === reason);
  const { code } = await transport.stop();
  assert.notEqual(code, 0);
  assert.equal(await close, code);
});

test('a failing stdin write rejects the pending operation and fences later calls', async t => {
  const { transport, child } = fixture(t);
  await transport.ready;
  child.stdin.destroy();
  const reason = await failure(transport.call(inspect), /stdin write failed.*outcome is unknown/);
  await assert.rejects(transport.call(inspect), error => error === reason);
  assert.deepEqual(await transport.stop(), { code: 0, signal: null });
});

for (const [fragment, expected] of [['', /stdout closed/], ['{"id":1,"ok":true,"result":{}}', /incomplete JSON line/]]) {
  test(`stdout EOF${fragment ? ' with an unterminated response' : ''} fails an operation while the child is live`, async t => {
    const { transport, child } = fixture(t);
    await transport.ready;
    const first = nextMessage(child);
    const rejected = failure(transport.call(inspect), expected);
    await first;
    if (fragment) {
      const data = once(child.stdout, 'data');
      child.send({ op: 'raw', value: fragment });
      await data;
    }
    child.send({ op: 'end' });
    const reason = await rejected;
    assert.match(reason.message, /outcome is unknown/);
    const live = nextMessage(child);
    child.send({ op: 'requests' });
    assert.deepEqual(await live, [{ id: 1, request: inspect }]);
    await assert.rejects(transport.call(inspect), error => error === reason);
    await transport.stop();
  });
}

test('framing counts UTF-8 bytes including the ready envelope', async t => {
  const { transport } = fixture(t, "reply({ ready: { payload: 'é'.repeat(32768) } });");
  await assert.rejects(transport.ready, /exceeds 64 KiB/);
  await transport.stop();
});

test('a complete final response settles its operation before child close', async t => {
  const { transport, closed } = fixture(t, String.raw`
    input.once('line', line => {
      const { id } = JSON.parse(line);
      const message = JSON.stringify({ id, ok: true, result: { final: true } });
      process.stdout.write(message + '\n', () => process.exit(0));
    });
    reply({ ready: {} });
  `);
  await transport.ready;
  assert.deepEqual(await transport.call(inspect), { final: true });
  assert.deepEqual(await closed, { code: 0, signal: null });
  await assert.rejects(transport.call(inspect), /stdout closed/);
  assert.deepEqual(await transport.stop(), { code: 0, signal: null });
});

test('the close promise marks an exited generation before a later controller reset', async t => {
  const { transport, child } = fixture(t, String.raw`
    input.once('line', line => {
      const { id } = JSON.parse(line);
      process.stdout.write(JSON.stringify({ id, ok: true, result: { final: true } }) + '\n', () => process.exit(23));
    });
    reply({ ready: {} });
  `);
  await transport.ready;
  const response = transport.call(inspect);
  await nextMessage(child);
  child.send({ op: 'reply', value: { id: 1, ok: true, result: { final: true } } });
  assert.deepEqual(await response, { final: true });
  assert.deepEqual(await transport.closed, { code: 23, signal: null });
  await assert.rejects(transport.call(inspect), error => /stdout closed|child process closed/.test(error.message));
});

test('stop settles pending work and waits for real close after SIGTERM', { skip: process.platform === 'win32' }, async t => {
  const { transport, child, closed } = fixture(t, `
    waitForSignal = true;
    process.on('SIGTERM', () => process.disconnect());
    reply({ ready: {} });
  `, { shutdownTimeoutsMs: [10, 1000, 1000] });
  await transport.ready;
  const first = nextMessage(child);
  const rejected = failure(transport.call(inspect), /Fixture stopped.*outcome is unknown/);
  await first;
  const stopped = transport.stop();
  assert.equal(transport.stop(), stopped);
  const reason = await rejected;
  await assert.rejects(transport.call(inspect), error => error === reason);
  assert.deepEqual(await stopped, { code: 0, signal: null });
  assert.deepEqual(await closed, { code: 0, signal: null });
});
