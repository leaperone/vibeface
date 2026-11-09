import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdir, rename, unlink, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { resolve } from 'node:path';

function authorized(value, token) {
  if (typeof value !== 'string') return false;
  const received = Buffer.from(value);
  const expected = Buffer.from(`Bearer ${token}`);
  return received.length === expected.length && timingSafeEqual(received, expected);
}

if (process.argv[2] === '--self-test') {
  const token = 'a'.repeat(64);
  assert(authorized(`Bearer ${token}`, token));
  for (const value of [undefined, '', [], `Bearer ${'b'.repeat(64)}`, `Bearer é${'a'.repeat(63)}`]) {
    assert.equal(authorized(value, token), false);
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
let pending;
let ready;
let changing = false;
let stopping = false;
class FixtureError extends Error {}

function object(value) {
  assert(value && typeof value === 'object' && !Array.isArray(value), 'Expected fixture object');
  return value;
}
async function stopFixture() {
  const previous = fixture;
  fixture = undefined;
  ready = undefined;
  if (!previous || previous.exitCode !== null) return;
  previous.stdin.end();
  await new Promise((done, fail) => {
    const timer = setTimeout(() => { previous.kill('SIGKILL'); fail(new Error('Fixture cleanup timed out')); }, 20000);
    previous.once('exit', code => { clearTimeout(timer); code === 0 ? done() : fail(new Error('Fixture cleanup failed')); });
  });
}
async function startFixture(kind) {
  assert(['single', 'delivery', 'workspace', 'bootstrap'].includes(kind), 'Unknown fixture kind');
  await stopFixture();
  const args = [resolve(repository, 'scripts/verify-node.py'), '--e2e-fixture', '--binary', binary,
    '--artifacts', root, '--fixture-port', String(port), '--fixture-kind', kind];
  if (cryptoModule) args.push('--crypto-module', cryptoModule);
  if (appDir) args.push('--app-dir', appDir);
  if (process.env.VIBEFACE_E2E_PROFILE_FILE) args.push('--fixture-profile', process.env.VIBEFACE_E2E_PROFILE_FILE);
  const child = spawn('python3', args, { cwd: repository, stdio: ['pipe', 'pipe', 'inherit'] });
  fixture = child;
  child.once('error', error => { pending?.reject(error); pending = undefined; });
  child.once('exit', code => {
    if (fixture === child) fixture = undefined;
    pending?.reject(new Error(`Fixture exited ${code ?? 'without a code'}`));
    pending = undefined;
  });
  const first = new Promise((resolveReady, reject) => { pending = { resolve: resolveReady, reject }; });
  createInterface({ input: child.stdout }).on('line', line => {
    const waiter = pending;
    try {
      assert(line.length <= 64 * 1024, 'Fixture response too large');
      const value = object(JSON.parse(line));
      pending = undefined;
      assert(waiter, 'Unexpected fixture response');
      if ('ready' in value) { ready = object(value.ready); waiter.resolve(ready); }
      else if (value.ok === true) waiter.resolve(value.result);
      else {
        const kind = typeof value.error === 'string' && /^[A-Za-z]+Error$/.test(value.error) ? value.error : 'Error';
        const frames = Array.isArray(value.frames) ? value.frames.filter(frame => typeof frame === 'string' && /^[A-Za-z0-9_.-]+:[0-9]+:[A-Za-z0-9_<>]+$/.test(frame)) : [];
        waiter.reject(new FixtureError(`Fixture ${kind}${frames.length ? ` at ${frames.join(', ')}` : ''}`));
      }
    } catch (error) { waiter?.reject(error); pending = undefined; child.kill('SIGTERM'); }
  });
  return first;
}
function command(value) {
  assert(fixture && fixture.exitCode === null && !pending, 'Fixture is unavailable or busy');
  const result = new Promise((resolveResult, reject) => { pending = { resolve: resolveResult, reject }; });
  fixture.stdin.write(`${JSON.stringify(value)}\n`);
  return result;
}
const control = createServer(async (request, response) => {
  if (request.method !== 'POST' || request.url !== '/' || !authorized(request.headers.authorization, token)) {
    response.writeHead(403).end(); return;
  }
  if (changing || stopping) { response.writeHead(409).end(); return; }
  changing = true;
  try {
    const chunks = [];
    let length = 0;
    for await (const chunk of request) { length += chunk.length; assert(length <= 4096, 'Fixture request too large'); chunks.push(chunk); }
    const value = object(JSON.parse(Buffer.concat(chunks).toString('utf8')));
    let result;
    if (value.op === 'reset') result = await startFixture(value.kind);
    else if (value.op === 'stop') { await stopFixture(); result = { stopped: true }; }
    else if (['restart', 'inspect', 'workspace_release', 'workspace_inspect', 'workspace_native_setup', 'bootstrap_info', 'bootstrap_pairing', 'bootstrap_approve'].includes(value.op)) result = await command(value);
    else throw new Error('Unknown fixture operation');
    response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(result));
  } catch (error) {
    response.writeHead(500, { 'content-type': 'application/json' }).end(JSON.stringify({ error: error instanceof FixtureError ? error.message : 'Fixture operation failed' }));
  } finally { changing = false; }
});
await new Promise((done, fail) => { control.once('error', fail); control.listen(0, '127.0.0.1', done); });
const address = control.address();
assert(address && typeof address === 'object');
const controlPath = `${root}/control/${port}.json`;
const temporary = `${controlPath}.${process.pid}.tmp`;
await writeFile(temporary, JSON.stringify({ url: `http://127.0.0.1:${address.port}`, token }), { mode: 0o600, flag: 'wx' });
await rename(temporary, controlPath);
async function shutdown() {
  if (stopping) return;
  stopping = true;
  control.close();
  control.closeAllConnections();
  try { await stopFixture(); await unlink(controlPath); }
  catch { process.exitCode = 1; }
}
process.once('SIGTERM', () => { void shutdown(); });
process.once('SIGINT', () => { void shutdown(); });
await startFixture('single');
