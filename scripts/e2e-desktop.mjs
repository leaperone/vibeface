// Launches the built desktop application for tests/e2e/e2e.desktop.config.ts and exposes a private
// control API: the real application, its own isolated profile, and its owned Core are inspected,
// quit and restarted through it. Only this driver's own child handle is ever terminated.
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdir, readFile, rename, unlink, writeFile } from 'node:fs/promises';
import { basename, join, resolve } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { promisify } from 'node:util';

const run = promisify(execFile);
const port = Number(process.argv[2]);
assert(Number.isInteger(port) && port > 0 && port < 65536, 'Expected a TCP port');
const repository = resolve(import.meta.dirname, '..');
const application = process.env.VIBEFACE_E2E_DESKTOP_APP;
assert(application, 'VIBEFACE_E2E_DESKTOP_APP names the built desktop application');
const windows = process.platform === 'win32';
const executable = windows ? application : join(application, 'Contents/MacOS/Vibeface');
const resources = windows ? join(application, '..', 'resources') : join(application, 'Contents/Resources');
const root = resolve(repository, '.artifacts/e2e/desktop');
// A personal directory really does carry spaces and characters outside ASCII.
const profile = join(root, 'profiles', `测试 配置 ${port}`);
let child;
let stopping = false;
let changing = false;

async function devtools(path) {
  const response = await fetch(`http://127.0.0.1:${port}${path}`, { signal: AbortSignal.timeout(10000) });
  assert(response.ok, `The DevTools endpoint answered ${response.status}`);
  return response.json();
}

/** Every direct child of the application, so an owned Core is distinguished from an attached one. */
async function descendants(pid) {
  const listing = windows
    ? await run('powershell', ['-NoProfile', '-NonInteractive', '-Command',
      `Get-CimInstance Win32_Process -Filter "ParentProcessId=${pid}" | ForEach-Object { "$($_.ProcessId) $($_.Name)" }`])
    : await run('ps', ['-ax', '-o', 'pid=,ppid=,comm=']);
  const children = [];
  for (const line of listing.stdout.split('\n')) {
    const fields = line.trim().split(/\s+/);
    const identifier = fields[0];
    const parent = windows ? String(pid) : fields[1];
    const command = (windows ? fields.slice(1) : fields.slice(2)).join(' ');
    if (!command || parent !== String(pid) || !/^[0-9]+$/.test(identifier ?? '')) continue;
    children.push({ pid: Number(identifier), name: basename(command) });
  }
  return children;
}

async function cores(pid) {
  const children = await descendants(pid);
  return children.filter(entry => /^vibeface-core(\.exe)?$/.test(entry.name)).map(entry => entry.pid);
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function start() {
  assert(!child || child.exitCode !== null, 'The desktop application is already running');
  await mkdir(profile, { recursive: true });
  let failure;
  const started = spawn(executable, ['--profile-dir', profile, `--remote-debugging-port=${port}`],
    { cwd: repository, stdio: ['ignore', 'inherit', 'inherit'] });
  child = started;
  started.once('error', error => { failure = error; });
  for (let attempt = 0; attempt < 120; attempt += 1) {
    if (failure) throw failure;
    assert(started.exitCode === null, `The desktop application exited ${started.exitCode} before opening a window`);
    try {
      await devtools('/json/version');
      return;
    } catch {
      await sleep(500);
    }
  }
  throw new Error('The desktop application never exposed its window');
}

/**
 * Quits the application the way a person does and waits for its own process to end, reporting the
 * Core processes it owned and any of them that outlived it.
 */
async function quit() {
  const running = child;
  child = undefined;
  if (!running || running.exitCode !== null) return { pid: running ? running.pid : null, owned: [], surviving: [] };
  const owned = await cores(running.pid);
  const exited = new Promise(done => running.once('exit', done));
  try {
    const { webSocketDebuggerUrl } = await devtools('/json/version');
    assert(typeof webSocketDebuggerUrl === 'string', 'The application exposes its own browser endpoint');
    const socket = new WebSocket(webSocketDebuggerUrl);
    await new Promise((done, fail) => {
      socket.addEventListener('open', () => {
        socket.send(JSON.stringify({ id: 1, method: 'Browser.close' }));
        done();
      });
      socket.addEventListener('error', () => fail(new Error('The browser endpoint refused the quit request')));
    });
    socket.close();
  } catch {
    // Only this driver's own child handle is ever signalled; no process is looked up by identifier.
    running.kill();
  }
  const limit = AbortSignal.timeout(30000);
  const timeout = new Promise((_, fail) => {
    limit.addEventListener('abort', () => fail(new Error('The desktop application did not quit')));
  });
  await Promise.race([exited, timeout]).catch(error => {
    running.kill('SIGKILL');
    throw error;
  });
  // An owned Core ends with the application that started it; a survivor would be reparented, not stopped.
  for (let attempt = 0; attempt < 20 && owned.some(alive); attempt += 1) await sleep(500);
  return { pid: running.pid, owned, surviving: owned.filter(alive) };
}

async function inspect() {
  assert(child && child.exitCode === null, 'The desktop application is running');
  const runtime = JSON.parse(await readFile(join(resources, 'runtime/BUILD-INFO.json'), 'utf8'));
  const pages = await devtools('/json/list');
  return {
    pid: child.pid,
    profile,
    runtime,
    cores: await cores(child.pid),
    pages: pages.map(({ type, url, title, webSocketDebuggerUrl }) => ({ type, url, title, webSocketDebuggerUrl })),
  };
}

await mkdir(join(root, 'control'), { recursive: true, mode: 0o700 });
const token = randomBytes(32).toString('hex');
const control = createServer(async (request, response) => {
  const authorization = request.headers.authorization;
  const expected = `Bearer ${token}`;
  if (request.method !== 'POST' || request.url !== '/' || typeof authorization !== 'string'
    || authorization.length !== expected.length || !timingSafeEqual(Buffer.from(authorization), Buffer.from(expected))) {
    response.writeHead(403).end();
    return;
  }
  if (changing || stopping) {
    response.writeHead(409).end();
    return;
  }
  changing = true;
  try {
    const chunks = [];
    let length = 0;
    for await (const chunk of request) {
      length += chunk.length;
      assert(length <= 4096, 'Control request too large');
      chunks.push(chunk);
    }
    const value = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    assert(value && typeof value === 'object' && !Array.isArray(value), 'Expected a control object');
    let result;
    if (value.op === 'inspect') result = await inspect();
    else if (value.op === 'quit') result = await quit();
    else if (value.op === 'start') { await start(); result = await inspect(); }
    else throw new Error('Unknown control operation');
    response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(result));
  } catch {
    response.writeHead(500, { 'content-type': 'application/json' }).end(JSON.stringify({ error: 'Control operation failed' }));
  } finally {
    changing = false;
  }
});
const controlPath = join(root, `control/${port}.json`);

async function shutdown() {
  if (stopping) return;
  stopping = true;
  control.close();
  control.closeAllConnections();
  try {
    await quit();
    await unlink(controlPath);
  } catch {
    process.exitCode = 1;
  }
}
process.once('SIGTERM', () => { void shutdown(); });
process.once('SIGINT', () => { void shutdown(); });
await new Promise((done, fail) => { control.once('error', fail); control.listen(0, '127.0.0.1', done); });
const address = control.address();
assert(address && typeof address === 'object');
try {
  await start();
} catch (error) {
  console.error(`The desktop application did not start: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
// The control file is published only once the window is up, so its presence means the application is ready.
const temporary = `${controlPath}.${process.pid}.tmp`;
await writeFile(temporary, JSON.stringify({ url: `http://127.0.0.1:${address.port}`, token }), { mode: 0o600, flag: 'wx' });
await rename(temporary, controlPath);
