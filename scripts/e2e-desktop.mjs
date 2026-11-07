// Drives the packaged application through its own DevTools endpoint. The app starts with a fresh
// profile and no Node. Fixture Nodes are initialized from this package's sealed runtime and only
// this driver's child handles and uniquely marked task directories are ever stopped or removed.
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdir, readFile, rename, rm, stat, unlink } from 'node:fs/promises';
import { basename, join, resolve, sep } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { promisify } from 'node:util';
import { ensurePrivateDirectory, resolveBoundedPath, writePrivateControlFile } from './private-control.mjs';

const run = promisify(execFile);
const port = Number(process.argv[2]);
assert(Number.isInteger(port) && port > 0 && port < 65536, 'Expected a TCP port');
const repository = resolve(import.meta.dirname, '..');
const application = process.env.VIBEFACE_E2E_DESKTOP_APP;
assert(application, 'VIBEFACE_E2E_DESKTOP_APP names the built desktop application');
const windows = process.platform === 'win32';
const executable = process.platform === 'darwin' ? join(application, 'Contents/MacOS/Vibeface') : application;
const resources = process.platform === 'darwin' ? join(application, 'Contents/Resources') : join(application, '..', 'resources');
const runtime = join(resources, 'runtime');
const cli = join(runtime, windows ? 'vibeface.exe' : 'vibeface');
const core = join(runtime, windows ? 'vibeface-core.exe' : 'vibeface-core');
const root = resolve(repository, '.artifacts/e2e/desktop');
const controlRoot = resolveBoundedPath(root, 'control');
const taskId = `${process.pid}-${port}-${randomBytes(6).toString('hex')}`;
// A personal directory carries spaces and characters outside ASCII, and lives outside the app profile.
const profile = join(root, 'profiles', `测试 配置 ${taskId}`);
const fixtureRoot = resolve(root, 'fixtures', taskId);
let child;
let stopping = false;
let changing = false;
const fixtures = new Map();

async function devtools(path) {
  const response = await fetch(`http://127.0.0.1:${port}${path}`, { signal: AbortSignal.timeout(10000) });
  assert(response.ok, `The DevTools endpoint answered ${response.status}`);
  return response.json();
}

/** Every direct child of the application, so a Core it owns is distinguished from an attached one. */
async function descendants(pid) {
  const listing = windows
    ? await run('powershell', ['-NoProfile', '-NonInteractive', '-Command',
      `Get-CimInstance Win32_Process -Filter "ParentProcessId=${pid}" | Select-Object ProcessId,Name,CommandLine | ConvertTo-Json -Compress`])
    : await run('ps', ['-ax', '-o', 'pid=,ppid=,args=']);
  const children = [];
  if (windows) {
    const processes = listing.stdout.trim() ? JSON.parse(listing.stdout) : [];
    for (const item of Array.isArray(processes) ? processes : processes ? [processes] : []) {
      if (typeof item.ProcessId === 'number' && typeof item.Name === 'string') {
        children.push({ pid: item.ProcessId, name: item.Name, commandLine: String(item.CommandLine ?? '') });
      }
    }
    return children;
  }
  for (const line of listing.stdout.split('\n')) {
    const match = line.match(/^\s*(\d+)\s+(\d+)\s+(.*)$/);
    if (!match || match[2] !== String(pid)) continue;
    const commandLine = match[3];
    const coreMatch = commandLine.match(/(?:^|\s|\/)vibeface-core(\.exe)?(?:\s|$)/);
    children.push({ pid: Number(match[1]), name: coreMatch ? `vibeface-core${coreMatch[1] ?? ''}` : basename(commandLine), commandLine });
  }
  return children;
}

async function cores(pid) {
  return (await descendants(pid))
    .filter(entry => /^vibeface-core(\.exe)?$/.test(entry.name)
      && /(?:^|\s)--data-dir(?:\s|=)/.test(entry.commandLine)
      && !/(?:^|\s)--control(?:\s|$)/.test(entry.commandLine))
    .map(entry => entry.pid);
}

function alive(pid) {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

/** Evaluate inside an existing Electron page; no second browser is created. */
async function inPage(page, expression) {
  const socket = new WebSocket(page.webSocketDebuggerUrl);
  try {
    return await new Promise((done, fail) => {
      const limit = AbortSignal.timeout(30000);
      limit.addEventListener('abort', () => fail(new Error('The application page did not answer')));
      socket.addEventListener('error', () => fail(new Error('The application page could not be reached')));
      socket.addEventListener('open', () => socket.send(JSON.stringify({ id: 1, method: 'Runtime.evaluate', params: { expression, returnByValue: true, awaitPromise: true } })));
      socket.addEventListener('message', event => {
        try {
          const message = JSON.parse(event.data);
          if (message.id !== 1) return;
          assert(!message.error && !message.result?.exceptionDetails,
            `The application page rejected the operation: ${message.error?.message ?? message.result?.exceptionDetails?.text ?? 'no result'}`);
          done(message.result.result.value);
        } catch (error) { fail(error); }
      });
    });
  } finally { socket.close(); }
}

async function launcherPage() {
  const pages = await devtools('/json/list');
  return pages.find(page => page.type === 'page' && page.url.startsWith('vibeface-launcher://app/'));
}

async function waitLauncher() {
  const deadline = Date.now() + 120000;
  let last = '';
  while (Date.now() < deadline) {
    assert(child && child.exitCode === null, 'The application exited before startup completed');
    const launcher = await launcherPage();
    if (launcher) {
      try {
        if (await inPage(launcher, `(async () => {
          if (document.readyState !== 'complete' || typeof window.vibefaceLauncher !== 'object') return false;
          const status = await window.vibefaceLauncher.getStatus();
          return ['ready', 'failed'].includes(status.discovery.kind) && status.backgroundNode.kind !== 'checking';
        })()`)) return;
        last = 'launcher preload is not ready';
      } catch (error) { last = error instanceof Error ? error.message : String(error); }
    }
    await sleep(100);
  }
  throw new Error(`The packaged desktop launcher did not become ready: ${last}`);
}

async function prepareProfileParent() {
  // The product must create and protect its own profile just as on a real first launch.
  await mkdir(join(profile, '..'), { recursive: true, mode: 0o700 });
}

async function start() {
  assert(!child || child.exitCode !== null, 'The desktop application is already running');
  await prepareProfileParent();
  const started = spawn(executable, ['--profile-dir', profile, `--remote-debugging-port=${port}`],
    { cwd: repository, stdio: 'ignore' });
  child = started;
  await new Promise((done, fail) => {
    started.once('error', fail);
    started.once('spawn', done);
  });
  let ready = false;
  for (let attempt = 0; attempt < 120; attempt += 1) {
    assert(started.exitCode === null, `The desktop application exited ${started.exitCode} before opening a window`);
    try { await devtools('/json/version'); ready = true; break; } catch { await sleep(500); }
  }
  assert(ready, 'The packaged desktop DevTools endpoint became ready');
  await waitLauncher();
}

/** Quit using the actual browser endpoint, and report only Core children owned by this app. */
async function quit() {
  const running = child;
  child = undefined;
  if (!running || running.exitCode !== null) return { pid: running ? running.pid : null, owned: [], surviving: [] };
  const owned = await cores(running.pid);
  const exited = new Promise(done => running.once('exit', done));
  try {
    const launcher = await launcherPage();
    assert(launcher, 'The trusted launcher is available for its explicit quit accelerator');
    const socket = new WebSocket(launcher.webSocketDebuggerUrl);
    const mac = process.platform === 'darwin';
    const key = mac ? 'q' : 'F4';
    const code = mac ? 'KeyQ' : 'F4';
    const modifiers = mac ? 4 : 1;
    await new Promise((done, fail) => {
      let replies = 0;
      socket.addEventListener('open', () => {
        socket.send(JSON.stringify({ id: 1, method: 'Input.dispatchKeyEvent', params: { type: 'keyDown', key, code, modifiers } }));
        socket.send(JSON.stringify({ id: 2, method: 'Input.dispatchKeyEvent', params: { type: 'keyUp', key, code, modifiers } }));
      });
      socket.addEventListener('message', event => {
        const message = JSON.parse(event.data);
        if (message.id !== 1 && message.id !== 2) return;
        if (message.error) { fail(new Error('The desktop quit accelerator was rejected')); return; }
        replies += 1;
        if (replies === 2) done();
      });
      socket.addEventListener('error', () => fail(new Error('The launcher rejected the quit accelerator')));
    });
    socket.close();
  } catch {
    // Close the window as a secondary user-facing route; the app's tray remains responsible for quit.
    try {
      const { webSocketDebuggerUrl } = await devtools('/json/version');
      const socket = new WebSocket(webSocketDebuggerUrl);
      await new Promise(done => { socket.addEventListener('open', () => { socket.send(JSON.stringify({ id: 1, method: 'Browser.close' })); done(); }); });
      socket.close();
    } catch { /* fall through to scoped process cleanup below */ }
  }
  let forced = false;
  const limit = AbortSignal.timeout(30000);
  const timeout = new Promise(resolve => limit.addEventListener('abort', () => resolve('timeout')));
  const result = await Promise.race([exited.then(() => 'exit'), timeout]);
  if (result === 'timeout') {
    forced = true;
    // Only this driver's own child handle is ever signalled, to avoid leaking a failed fixture.
    running.kill();
  }
  if (forced) await Promise.race([exited, new Promise((_, fail) => setTimeout(() => fail(new Error('The desktop application did not exit after task-scoped cleanup')), 30000))]);
  for (let attempt = 0; attempt < 20 && owned.some(alive); attempt += 1) await sleep(500);
  return { pid: running.pid, owned, surviving: owned.filter(alive), forced };
}

function pageSummary(pages) {
  return pages.map(({ type, url, title, webSocketDebuggerUrl }) => ({ type, url, title, webSocketDebuggerUrl }));
}

async function status() {
  assert(child && child.exitCode === null, 'The desktop application is running');
  const launcher = await launcherPage();
  assert(launcher, 'The trusted launcher window exists');
  return inPage(launcher, 'window.vibefaceLauncher.getStatus()');
}

async function closeWindows() {
  assert(child && child.exitCode === null, 'The desktop application is running');
  const pages = (await devtools('/json/list')).filter(page => page.type === 'page');
  const results = [];
  for (const page of pages) {
    const modifiers = process.platform === 'darwin' ? 4 : 2;
    const socket = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise((done, fail) => {
      let replies = 0;
      socket.addEventListener('open', () => {
        socket.send(JSON.stringify({ id: 1, method: 'Input.dispatchKeyEvent', params: { type: 'keyDown', key: 'w', code: 'KeyW', modifiers } }));
        socket.send(JSON.stringify({ id: 2, method: 'Input.dispatchKeyEvent', params: { type: 'keyUp', key: 'w', code: 'KeyW', modifiers } }));
      });
      socket.addEventListener('message', event => {
        const message = JSON.parse(event.data);
        if (message.id !== 1 && message.id !== 2) return;
        if (message.error) { fail(new Error('The close-window accelerator was rejected')); return; }
        if (++replies === 2) done();
      });
      socket.addEventListener('error', () => fail(new Error('The window refused its close accelerator')));
    });
    socket.close();
    let visibility = 'visible';
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline && visibility !== 'hidden') {
      visibility = await inPage(page, 'document.visibilityState');
      if (visibility !== 'hidden') await sleep(100);
    }
    results.push({ url: page.url, visibility });
  }
  return results;
}

async function invoke(method, args = []) {
  assert(child && child.exitCode === null, 'The desktop application is running');
  const launcher = await launcherPage();
  assert(launcher, 'The trusted launcher window exists');
  const allowed = new Set(['chooseNodeMode', 'discoverNodes', 'connectDiscoveredNode', 'startDiscoveredNode',
    'startOwned', 'stopOwned', 'openBusiness', 'installBackgroundNode', 'startBackgroundNode',
    'stopBackgroundNode', 'removeBackgroundNode']);
  assert(allowed.has(method), 'Only the fixed public launcher API can be invoked');
  const encoded = JSON.stringify(args);
  return inPage(launcher, `window.vibefaceLauncher[${JSON.stringify(method)}](...${encoded})`);
}

async function inspect() {
  assert(child && child.exitCode === null, 'The desktop application is running');
  const runtimeInfo = JSON.parse(await readFile(join(runtime, 'BUILD-INFO.json'), 'utf8'));
  const pages = await devtools('/json/list');
  return { pid: child.pid, profile, runtime: runtimeInfo, cores: await cores(child.pid), status: await status(), pages: pageSummary(pages) };
}

async function freePort() {
  const server = createServer();
  try {
    await new Promise((done, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', done); });
    const address = server.address(); assert(address && typeof address === 'object'); return address.port;
  } finally { await new Promise(done => server.close(done)); }
}

async function controlCore(request) {
  const process = spawn(core, ['--control'], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  const stdout = []; const stderr = [];
  process.stdout.on('data', chunk => stdout.push(chunk));
  process.stderr.on('data', chunk => stderr.push(chunk));
  process.stdin.end(`${JSON.stringify(request)}\n`);
  const [code] = await new Promise((done, fail) => { process.once('error', fail); process.once('close', (code, signal) => signal ? fail(new Error('Bundled Core control was signalled')) : done([code])); });
  assert.equal(code, 0, `Bundled Core control succeeded (${Buffer.concat(stderr).length} diagnostic bytes)`);
  const response = JSON.parse(Buffer.concat(stdout).toString('utf8').split('\n', 1)[0] ?? 'null');
  assert(response?.ok, `Bundled Core accepted ${request.command}`);
  return response.result;
}

async function createFixture(startNow = true) {
  const index = fixtures.size + 1;
  const dataDir = join(fixtureRoot, `节点 Fixture ${index}`);
  const peerPort = await freePort(); const localPort = await freePort();
  await mkdir(fixtureRoot, { recursive: true });
  await run(cli, ['init', '--data-dir', dataDir, '--listen', `127.0.0.1:${peerPort}`,
    '--local-listen', `127.0.0.1:${localPort}`, '--bootstrap', 'off'], { cwd: runtime });
  const inspected = await controlCore({ command: 'inspect', data_dir: dataDir });
  const fixture = { dataDir: inspected.data_dir, peerPort, localPort, deviceId: inspected.device_id, child: undefined };
  assert(typeof fixture.dataDir === 'string', 'Core reports the canonical fixture directory');
  fixtures.set(fixture.dataDir, fixture);
  if (startNow) await startFixture(fixture.dataDir);
  return { dataDir: fixture.dataDir, peerPort, localPort, deviceId: fixture.deviceId, running: Boolean(fixture.child) };
}

async function startFixture(dataDir) {
  const fixture = fixtures.get(dataDir);
  assert(fixture, 'Fixture Node belongs to this driver task');
  assert(!fixture.child || fixture.child.exitCode !== null, 'Fixture Node is not already running');
  fixture.child = spawn(core, ['--data-dir', dataDir], { cwd: runtime, stdio: ['ignore', 'ignore', 'ignore'], windowsHide: true });
  fixture.child.once('error', error => { fixture.startError = error; });
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    assert(!fixture.startError, 'The task-owned fixture Core started');
    assert(fixture.child.exitCode === null, 'The fixture Core remains running while its health is checked');
    const inspected = await controlCore({ command: 'inspect', data_dir: dataDir });
    if (inspected.status === 'running' && inspected.device_id === fixture.deviceId) return inspected;
    await sleep(100);
  }
  throw new Error(`Task-owned fixture Node ${fixture.deviceId} did not report healthy`);
}

async function stopFixture(dataDir) {
  const fixture = fixtures.get(dataDir);
  assert(fixture, 'Fixture Node belongs to this driver task');
  const process = fixture.child;
  if (process && process.exitCode === null) {
    const exited = new Promise(done => process.once('exit', done));
    process.kill('SIGTERM');
    const timeout = new Promise((_, fail) => setTimeout(() => fail(new Error('Task-owned fixture Node did not stop')), 30000));
    await Promise.race([exited, timeout]);
  }
  fixture.child = undefined;
  const inspected = await controlCore({ command: 'inspect', data_dir: dataDir });
  assert.equal(inspected.status, 'stopped', 'Only the fixture we started is stopped');
  return inspected;
}

async function snapshotNode(dataDir) {
  const fields = await controlCore({ command: 'inspect', data_dir: dataDir });
  const values = {};
  for (const name of ['config.json', 'identity.json', 'data-epoch.json']) {
    try { values[name] = createHash('sha256').update(await readFile(join(dataDir, name))).digest('hex'); }
    catch (error) { if (error?.code !== 'ENOENT') throw error; values[name] = null; }
  }
  return { deviceId: fields.device_id, status: fields.status, version: fields.version, coreVersion: fields.core_version, values };
}

async function nativeServiceStatus(expectedDeviceId) {
  let executable = cli;
  try { await stat(executable); }
  catch (error) {
    if (error?.code !== 'ENOENT') throw error;
    executable = join(profile, 'node-runtime', 'current', windows ? 'vibeface.exe' : 'vibeface');
  }
  const result = await run(executable, ['desktop-node', '--profile-dir', profile, '--expected-device-id', expectedDeviceId, 'status'], { cwd: profile });
  return JSON.parse(result.stdout.trim());
}

async function detachSourceRuntime(expectedDeviceId) {
  assert(process.platform === 'linux', 'Only the Linux AppImage runtime source is detached in this scenario');
  assert(!child || child.exitCode !== null, 'The application quit before its package runtime is detached');
  const detached = `${runtime}.detached-${taskId}`;
  await rename(runtime, detached);
  try {
    const service = await nativeServiceStatus(expectedDeviceId);
    assert.equal(service.state, 'installed', 'the registered background service remains healthy without the AppImage runtime path');
    assert.equal(service.service_running, true);
    assert.equal(service.device_id, expectedDeviceId);
    return service;
  } finally {
    await rename(detached, runtime);
  }
}

async function cleanup() {
  for (const [dataDir, fixture] of fixtures) {
    if (fixture.child && fixture.child.exitCode === null) await stopFixture(dataDir);
  }
  // The service scenarios remove registrations through the app before task cleanup. Never delete
  // an active or non-task-owned fixture; the unique root is verified before recursive removal.
  try {
    const marker = await stat(fixtureRoot);
    assert(marker.isDirectory() && fixtureRoot.startsWith(join(root, 'fixtures') + sep), 'Fixture cleanup remains under this task root');
    await rm(fixtureRoot, { recursive: true, force: true });
  } catch (error) { if (error?.code !== 'ENOENT') throw error; }
  // A failed assertion can leave this task's optional service installed. Read its private receipt
  // under this unique profile, remove only that registration, and prove the manager no longer lists it.
  const receiptPath = join(profile, 'node-runtime', 'desktop-node.json');
  try {
    const receipt = JSON.parse(await readFile(receiptPath, 'utf8'));
    assert(typeof receipt.device_id === 'string' && typeof receipt.service_id === 'string');
    const state = await nativeServiceStatus(receipt.device_id);
    if (state.state !== 'not_installed') {
      await run(cli, ['desktop-node', '--profile-dir', profile, '--expected-device-id', receipt.device_id, 'uninstall'], { cwd: runtime });
      assert.equal((await nativeServiceStatus(receipt.device_id)).state, 'not_installed', 'task-owned current-user service is unregistered before task data cleanup');
    }
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
  await rm(profile, { recursive: true, force: true });
}

await ensurePrivateDirectory(controlRoot, { root });
await prepareProfileParent();
const token = randomBytes(32).toString('hex');
const control = createServer(async (request, response) => {
  const authorization = request.headers.authorization;
  const expected = `Bearer ${token}`;
  if (request.method !== 'POST' || request.url !== '/' || typeof authorization !== 'string'
    || authorization.length !== expected.length || !timingSafeEqual(Buffer.from(authorization), Buffer.from(expected))) {
    response.writeHead(403).end(); return;
  }
  if (changing || stopping) { response.writeHead(409).end(); return; }
  changing = true;
  try {
    const chunks = []; let length = 0;
    for await (const chunk of request) { length += chunk.length; assert(length <= 8192, 'Control request too large'); chunks.push(chunk); }
    const value = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    assert(value && typeof value === 'object' && !Array.isArray(value), 'Expected a control object');
    let result;
    if (value.op === 'inspect') result = await inspect();
    else if (value.op === 'status') result = await status();
    else if (value.op === 'close-windows') result = await closeWindows();
    else if (value.op === 'invoke') result = await invoke(value.method, value.args ?? []);
    else if (value.op === 'quit') result = await quit();
    else if (value.op === 'start') { await start(); result = await inspect(); }
    else if (value.op === 'create-fixture') result = await createFixture(value.start !== false);
    else if (value.op === 'start-fixture') result = await startFixture(value.dataDir);
    else if (value.op === 'stop-fixture') result = await stopFixture(value.dataDir);
    else if (value.op === 'snapshot-node') result = await snapshotNode(value.dataDir);
    else if (value.op === 'service-status') result = await nativeServiceStatus(value.deviceId);
    else if (value.op === 'detach-source-runtime') result = await detachSourceRuntime(value.deviceId);
    else if (value.op === 'cleanup') { await cleanup(); result = { cleaned: true }; }
    else throw new Error('Unknown control operation');
    response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(result));
  } catch (error) {
    response.writeHead(500, { 'content-type': 'application/json' }).end(JSON.stringify({ error: error instanceof Error ? error.message : 'Control operation failed' }));
  } finally { changing = false; }
});
const controlPath = resolveBoundedPath(controlRoot, `${port}.json`);

async function shutdown() {
  if (stopping) return;
  stopping = true;
  control.close(); control.closeAllConnections();
  try {
    await quit();
    await cleanup();
    await unlink(controlPath);
  } catch { process.exitCode = 1; }
}
process.once('SIGTERM', () => { void shutdown(); });
process.once('SIGINT', () => { void shutdown(); });
await new Promise((done, fail) => { control.once('error', fail); control.listen(0, '127.0.0.1', done); });
const address = control.address();
assert(address && typeof address === 'object');
try { await start(); }
catch (error) {
  console.error(`The packaged desktop launcher did not start: ${error instanceof Error ? error.message : String(error)}`);
  await shutdown(); process.exit(1);
}
await writePrivateControlFile(controlPath, JSON.stringify({ url: `http://127.0.0.1:${address.port}`, token }),
  { root: controlRoot, maxBytes: 2048 });
