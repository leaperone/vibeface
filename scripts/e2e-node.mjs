import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdir, rename, unlink, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { parseFixtureRequest } from '../tests/support/fixture-contract.ts';
import { withControlledFixture } from '../tests/support/fixture-lifecycle.ts';
import { createFixtureTransport, FixtureOperationError } from '../tests/support/fixture-transport.ts';

function authorized(value, token) {
  if (typeof value !== 'string') return false;
  const received = Buffer.from(value);
  const expected = Buffer.from(`Bearer ${token}`);
  return received.length === expected.length && timingSafeEqual(received, expected);
}

async function readControlRequest(request, kind) {
  const chunks = [];
  let length = 0;
  for await (const chunk of request) {
    length += chunk.length;
    assert(length <= 4096, 'Fixture request too large');
    chunks.push(chunk);
  }
  const json = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks));
  return parseFixtureRequest(JSON.parse(json), 'web', kind);
}

if (process.argv[2] === '--self-test') {
  const token = 'a'.repeat(64);
  assert(authorized(`Bearer ${token}`, token));
  for (const value of [undefined, '', [], `Bearer ${'b'.repeat(64)}`, `Bearer é${'a'.repeat(63)}`]) {
    assert.equal(authorized(value, token), false);
  }
  const reset = { op: 'reset', kind: 'delivery' };
  assert.deepEqual(await readControlRequest([Buffer.from(JSON.stringify(reset))], undefined), reset);
  for (const body of [[], [Buffer.alloc(4096), Buffer.from('x')], [Buffer.from([255])],
    [Buffer.from('{"op":"native_join"}')], [Buffer.from('{"op":"stop","role":"source"}')]]) {
    await assert.rejects(readControlRequest(body, 'delivery'));
  }
  console.log('e2e controller credential self-test passed');
  process.exit(0);
}

const port = Number(process.argv[2]);
assert(Number.isInteger(port) && port > 0 && port < 65536, 'Expected a TCP port');
const repository = resolve(import.meta.dirname, '..');
const binary = process.env.VIBEFACE_E2E_BINARY ?? 'vibeface';
const appDir = process.env.VIBEFACE_E2E_APP_DIR;
const cryptoModule = process.env.VIBEFACE_E2E_CRYPTO_MODULE
  || (appDir ? resolve(appDir, 'vibeface_secure.js') : undefined);
const root = resolve(repository, '.artifacts/e2e');
await mkdir(`${root}/control`, { recursive: true, mode: 0o700 });
const token = randomBytes(32).toString('hex');
let fixture;
let changing = false;
let stopping = false;

async function stopFixture() {
  const previous = fixture;
  if (!previous) return;
  const { code, signal } = await previous.transport.stop();
  if (fixture === previous) fixture = undefined;
  assert(code === 0 && signal === null, 'Fixture cleanup failed');
}
async function startFixture(kind) {
  await stopFixture();
  assert(!stopping, 'Fixture controller is shutting down');
  const args = [resolve(repository, 'scripts/verify-node.py'), '--e2e-fixture', '--binary', binary,
    '--artifacts', root, '--fixture-port', String(port), '--fixture-kind', kind];
  if (cryptoModule) args.push('--crypto-module', cryptoModule);
  if (appDir) args.push('--app-dir', appDir);
  if (process.env.VIBEFACE_E2E_PROFILE_FILE) args.push('--fixture-profile', process.env.VIBEFACE_E2E_PROFILE_FILE);
  const child = spawn('python3', args, { cwd: repository, stdio: ['pipe', 'pipe', 'inherit'] });
  const transport = createFixtureTransport(child, {
    kind, artifacts: root, origin: `http://127.0.0.1:${port}`, surface: 'web',
    requestTimeoutMs: 85000,
    shutdownTimeoutsMs: [15000, 5000, 5000],
  });
  fixture = { kind, transport };
  void transport.closed.then(() => {
    if (fixture?.transport === transport) fixture = undefined;
  });
  try {
    return await transport.ready;
  } catch (error) {
    return withControlledFixture({ run: async () => { throw error; }, stop: stopFixture });
  }
}
const control = createServer(async (request, response) => {
  if (request.method !== 'POST' || request.url !== '/' || !authorized(request.headers.authorization, token)) {
    response.writeHead(403).end(); return;
  }
  if (changing || stopping) { response.writeHead(409).end(); return; }
  changing = true;
  try {
    const value = await readControlRequest(request, fixture?.kind);
    let result;
    if (value.op === 'reset') result = await startFixture(value.kind);
    else if (value.op === 'stop') { await stopFixture(); result = { stopped: true }; }
    else { assert(fixture, 'Fixture unavailable'); result = await fixture.transport.call(value); }
    response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(result));
  } catch (error) {
    response.writeHead(500, { 'content-type': 'application/json' }).end(JSON.stringify({ error: error instanceof FixtureOperationError ? error.message : 'Fixture operation failed' }));
  } finally { changing = false; }
});
await new Promise((done, fail) => { control.once('error', fail); control.listen(0, '127.0.0.1', done); });
const address = control.address();
assert(address && typeof address === 'object');
const controlPath = `${root}/control/${port}.json`;
const temporary = `${controlPath}.${process.pid}.tmp`;
await writeFile(temporary, JSON.stringify({ url: `http://127.0.0.1:${address.port}`, token }), { mode: 0o600, flag: 'wx' });
await rename(temporary, controlPath);
let shutdownPromise;
function shutdown() {
  if (shutdownPromise) return shutdownPromise;
  stopping = true;
  control.close();
  control.closeAllConnections();
  shutdownPromise = withControlledFixture({ run: stopFixture, stop: () => unlink(controlPath) });
  return shutdownPromise;
}
function terminate() {
  void shutdown().catch(() => { process.exitCode = 1; });
}
process.once('SIGTERM', terminate);
process.once('SIGINT', terminate);
changing = true;
try {
  await startFixture('single');
} catch (error) {
  if (!stopping) await withControlledFixture({ run: async () => { throw error; }, stop: shutdown });
} finally {
  changing = false;
}
