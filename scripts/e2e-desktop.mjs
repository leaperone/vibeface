// Drives the packaged application through its own DevTools endpoint. The app starts with a fresh
// profile and no Node. Fixture Nodes are initialized from this package's sealed runtime and only
// this driver's child handles and uniquely marked task directories are ever stopped or removed.
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { createServer as createNetServer } from 'node:net';
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
let inspectorPort;
let stopping = false;
let changing = false;
const fixtures = new Map();

async function devtools(path) {
  const response = await fetchBounded(`http://127.0.0.1:${port}${path}`, 10000);
  assert(response.ok, `The DevTools endpoint answered ${response.status}`);
  return response.json();
}

async function fetchBounded(url, timeoutMs) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try { return await fetch(url, { signal: controller.signal }); }
  finally { clearTimeout(timer); }
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

/** Send one bounded CDP operation and close its socket and timer on every outcome. */
async function devtoolsCommand(endpoint, method, params, timeoutMs = 30000) {
  const socket = new WebSocket(endpoint);
  try {
    return await new Promise((done, fail) => {
      let timer;
      const finish = (error, value) => {
        clearTimeout(timer);
        socket.removeEventListener('open', onOpen);
        socket.removeEventListener('message', onMessage);
        socket.removeEventListener('error', onError);
        socket.removeEventListener('close', onClose);
        if (error) fail(error); else done(value);
      };
      const onOpen = () => socket.send(JSON.stringify({ id: 1, method, params }));
      const onMessage = event => {
        try {
          const message = JSON.parse(event.data);
          if (message.id !== 1) return;
          assert(!message.error, `${method} failed: ${message.error?.message ?? 'unknown DevTools error'}`);
          finish(null, message.result);
        } catch (error) { finish(error); }
      };
      const onError = () => finish(new Error(`${method} could not reach its owned application target`));
      const onClose = () => finish(new Error(`${method} target closed before answering`));
      socket.addEventListener('open', onOpen);
      socket.addEventListener('message', onMessage);
      socket.addEventListener('error', onError);
      socket.addEventListener('close', onClose);
      timer = setTimeout(() => finish(new Error(`${method} timed out after ${timeoutMs} ms`)), timeoutMs);
    });
  } finally {
    if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) socket.close();
  }
}

/** Evaluate in a renderer page for read-only UI and preload assertions. */
async function inPage(page, expression) {
  const result = await devtoolsCommand(page.webSocketDebuggerUrl, 'Runtime.evaluate', {
    expression, returnByValue: true, awaitPromise: true,
  });
  assert(result && !result.exceptionDetails,
    `The application page rejected the evaluation: ${result?.exceptionDetails?.text ?? 'no result'}`);
  return result.result?.value;
}

async function freePort() {
  const server = createNetServer();
  try {
    await new Promise((done, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', done); });
    const address = server.address();
    assert(address && typeof address === 'object');
    return address.port;
  } finally { await new Promise(done => server.close(done)); }
}

async function inspectorTargets() {
  assert(Number.isInteger(inspectorPort), 'The app-owned inspector port is assigned');
  const response = await fetchBounded(`http://127.0.0.1:${inspectorPort}/json/list`, 10000);
  assert(response.ok, 'The app-owned Node inspector answered its target listing');
  const targets = await response.json();
  assert(Array.isArray(targets));
  const main = targets.filter(target => target.type === 'node');
  assert.equal(main.length, 1, 'the dedicated inspector port exposes only this app main process');
  const endpoint = new URL(main[0].webSocketDebuggerUrl);
  assert(endpoint.protocol === 'ws:' && endpoint.hostname === '127.0.0.1' && endpoint.port === String(inspectorPort),
    'the app main inspector stays on its dedicated literal-loopback port');
  return main[0];
}

async function inMain(expression) {
  const target = await inspectorTargets();
  const result = await devtoolsCommand(target.webSocketDebuggerUrl, 'Runtime.evaluate', {
    expression, returnByValue: true, awaitPromise: true,
  });
  assert(result && !result.exceptionDetails,
    `The owned Electron main process rejected the evaluation: ${result?.exceptionDetails?.text ?? 'no result'}`);
  return result.result?.value;
}

async function waitForExit(process, timeoutMs) {
  if (process.exitCode !== null || process.signalCode !== null) return true;
  return new Promise(resolve => {
    let settled = false;
    const finish = exited => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      process.off('exit', onExit);
      resolve(exited);
    };
    const onExit = () => finish(true);
    const timer = setTimeout(() => finish(false), timeoutMs);
    process.once('exit', onExit);
    if (process.exitCode !== null || process.signalCode !== null) finish(true);
  });
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
  inspectorPort = await freePort();
  const started = spawn(executable, [`--inspect=127.0.0.1:${inspectorPort}`, '--profile-dir', profile, `--remote-debugging-port=${port}`],
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
  let inspected = false;
  for (let attempt = 0; attempt < 120; attempt += 1) {
    assert(started.exitCode === null, `The desktop application exited ${started.exitCode} before main inspection started`);
    try {
      const pid = await inMain('process.pid');
      assert.equal(pid, started.pid, 'main-process inspection is bound to the driver-owned Electron PID');
      inspected = true;
      break;
    } catch { await sleep(500); }
  }
  assert(inspected, 'The app-owned loopback main-process inspector became ready');
  await waitLauncher();
}

/** Invoke the product quit menu in this app's main process and report only its owned Core children. */
async function quit() {
  const running = child;
  if (!running || running.exitCode !== null || running.signalCode !== null) {
    child = undefined;
    inspectorPort = undefined;
    return { pid: running ? running.pid : null, owned: [], surviving: [], forced: Boolean(running) };
  }
  const owned = await cores(running.pid);
  assert(Number.isInteger(inspectorPort), 'the app-owned inspector is available for its explicit menu quit');
  let forced = false;
  const actionKey = `__vibefaceE2EQuit_${randomBytes(12).toString('hex')}`;
  try {
    const validated = await inMain(`(() => {
      const { Menu, BrowserWindow } = process.mainModule.require('electron');
      const menu = Menu.getApplicationMenu();
      if (!menu) throw new Error('application menu is unavailable');
      const pending = [...menu.items];
      let quitItem;
      while (pending.length) {
        const item = pending.pop();
        if (item.label === '退出 Vibeface') { quitItem = item; break; }
        if (item.submenu) pending.push(...item.submenu.items);
      }
      if (!quitItem || typeof quitItem.click !== 'function') throw new Error('the product quit menu item has no click handler');
      const focused = BrowserWindow.getFocusedWindow();
      globalThis[${JSON.stringify(actionKey)}] = () => quitItem.click(null, focused, focused?.webContents);
      return JSON.stringify({ pid: process.pid, label: quitItem.label, handler: typeof quitItem.click });
    })()`);
    assert.equal(typeof validated, 'string');
    const action = JSON.parse(validated);
    assert.equal(action.pid, running.pid, 'the product menu handler ran in the driver-owned app process');
    assert.equal(action.label, '退出 Vibeface', 'the real product quit menu item handled the request');
    assert.equal(action.handler, 'function', 'the receipt identifies the actual product menu click handler');

    let scheduled;
    let scheduleError;
    try {
      scheduled = await inMain(`(() => {
      const click = globalThis[${JSON.stringify(actionKey)}];
      if (typeof click !== 'function') throw new Error('validated product quit handler is unavailable');
      setImmediate(() => {
        delete globalThis[${JSON.stringify(actionKey)}];
        click();
      });
      return JSON.stringify({ pid: process.pid, label: '退出 Vibeface', scheduled: true });
      })()`);
    } catch (error) { scheduleError = error; }
    if (scheduled !== undefined) {
      assert.equal(typeof scheduled, 'string');
      const receipt = JSON.parse(scheduled);
      assert.equal(receipt.pid, running.pid, 'the handler was scheduled in the driver-owned Electron process');
      assert.equal(receipt.label, '退出 Vibeface');
      assert.equal(receipt.scheduled, true);
    }

    const exited = await waitForExit(running, 30000);
    const normalExit = exited && running.exitCode === 0 && running.signalCode === null;
    if (scheduled === undefined && !normalExit) throw scheduleError ?? new Error('the app did not confirm its scheduled product quit');
    forced = !normalExit;
  } catch (error) {
    forced = true;
    console.error(`The product quit menu did not complete normally: ${error instanceof Error ? error.message : String(error)}`);
  }
  // A failed product quit still gets task-scoped cleanup, but is reported as forced and fails acceptance.
  if (forced) {
    running.kill();
    assert(await waitForExit(running, 5000), 'the driver-owned app process exited after fallback cleanup');
  }
  child = undefined;
  inspectorPort = undefined;
  let surviving = owned.filter(alive);
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline && surviving.length) {
    await sleep(100);
    surviving = owned.filter(alive);
  }
  assert.deepEqual(surviving, [], 'normal product quit stopped every Core child owned by this application');
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
  const result = await inMain(`(() => {
    const { BrowserWindow } = process.mainModule.require('electron');
    const windows = BrowserWindow.getAllWindows();
    const before = windows.map(window => ({ window, url: window.webContents.getURL() }));
    for (const entry of before) entry.window.close();
    return JSON.stringify(before.map(({ window, url }) => ({ url, visibility: window.isVisible() ? 'visible' : 'hidden' })));
  })()`);
  assert.equal(typeof result, 'string');
  return JSON.parse(result);
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
    process.kill('SIGTERM');
    assert(await waitForExit(process, 30000), 'Task-owned fixture Node stopped within its existing cleanup bound');
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
