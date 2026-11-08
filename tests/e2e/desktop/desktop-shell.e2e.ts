import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { access, mkdir, readFile, realpath, stat, writeFile } from 'node:fs/promises';
import { isAbsolute, join, relative, resolve, sep, toNamespacedPath } from 'node:path';
import { expect } from 'e2e';
import { evaluate, screenshot, test, type Inspection, type Page } from '../desktop';

const ELECTRON_VERSION = '45.2.3';
const BUILD_INFO_FIELDS = ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
  'core_version', 'core_source_commit', 'app_version', 'app_source_commit'];
const DESKTOP_ARTIFACTS = resolve(import.meta.dirname, '../.artifacts/desktop');

async function assertManagedPackageUnderReleases(managedRoot: string): Promise<string> {
  const [releasesRoot, currentPath] = await Promise.all([
    realpath(join(managedRoot, 'releases')),
    realpath(join(managedRoot, 'current')),
  ]);
  const [releasesStat, currentStat] = await Promise.all([
    stat(releasesRoot, { bigint: true }),
    stat(currentPath, { bigint: true }),
  ]);
  assert(releasesStat.isDirectory(), 'the managed runtime keeps a releases directory');
  assert(currentStat.isDirectory(), 'the managed runtime current entry resolves to a directory');
  const underReleases = relative(releasesRoot, currentPath);
  assert(underReleases.length > 0 && underReleases !== '..'
    && !underReleases.startsWith(`..${sep}`) && !isAbsolute(underReleases),
    'the registered service executes a versioned copy under its profile-managed runtime root');
  return currentPath;
}

async function persistWindowsDiscoveryCensus(
  desktop: { discoveryCensus(remembered?: string[]): Promise<Record<string, unknown>> },
  remembered: string[],
) {
  if (process.platform !== 'win32') return;
  const census = await desktop.discoveryCensus(remembered);
  await mkdir(DESKTOP_ARTIFACTS, { recursive: true });
  await writeFile(resolve(DESKTOP_ARTIFACTS, `windows-discovery-census-${remembered.length}.json`), `${JSON.stringify(census, null, 2)}\n`);
}

function isClientUrl(raw: string): boolean {
  const url = new URL(raw);
  return url.protocol === 'vibeface-app:' && url.host === 'app';
}

function clientWindow(current: Inspection): Page {
  const pages = current.pages.filter(entry => entry.type === 'page');
  const clients = pages.filter(entry => isClientUrl(entry.url));
  assert.equal(clients.length, 1, 'exactly one trusted client window exists');
  const client = clients[0];
  assert(client);
  const foreign = pages.filter(entry => entry.type === 'page' && !isClientUrl(entry.url));
  assert.equal(foreign.length, 0, 'no extra browser windows are opened for desktop node control');
  return client;
}

async function text(target: Page, expression: string) {
  const value = await evaluate(target, expression);
  assert(typeof value === 'string', `${expression} reads as text`);
  return value;
}

async function noPath(path: string, message: string) {
  await assert.rejects(access(path), { code: 'ENOENT' }, message);
}

async function openNodePanel(page: Page) {
  await evaluate(page, 'window.vibefaceClient.showNodePanel()');
  await expect.poll(() => evaluate(page, 'location.pathname'), { timeout: 30000 }).toBe('/settings/desktop-node');
}

async function connectButtons(page: Page) {
  return Number(await evaluate(page, '[...document.querySelectorAll("button")].filter((node) => node.textContent?.trim() === "连接").length'));
}

test('a fresh packaged desktop is a controller without initializing a Node', async ({ desktop }) => {
  const before = await desktop.inspect();
  const runtime = JSON.parse(await readFile(join(process.env.VIBEFACE_E2E_DESKTOP_RUNTIME ?? '', 'BUILD-INFO.json'), 'utf8'));
  assert.deepEqual(Object.keys(before.runtime).sort(), [...BUILD_INFO_FIELDS].sort());
  assert.deepEqual(before.runtime, runtime, 'the bundled runtime is the verified package, byte for byte');
  assert.equal(before.cores.length, 0, 'the packaged app starts without spawning a Node');
  assert.equal(before.status.clientAvailable, true, 'the packaged desktop client does not depend on starting a Node');
  assert.match(before.profile, /[^\x20-\x7e]/, 'the profile path carries characters outside ASCII');
  assert.match(before.profile, / /, 'the profile path carries a space');
  assert.equal(before.status.core.kind, 'none');
  assert.equal(before.status.backgroundNode.kind, 'absent', 'the product prepares its own private profile before querying managed Node state');
  assert.equal(before.status.nodeMode, 'undecided');
  await noPath(join(before.profile, 'node'), 'a fresh controller profile has no Node data directory');

  const page = clientWindow(before);
  const agent = await text(page, 'navigator.userAgent');
  assert.match(agent, new RegExp(`Electron/${ELECTRON_VERSION.replace(/\./g, '\\.')}`));
  for (const global of ['require', 'process', 'module', 'Buffer', 'global', '__dirname']) {
    expect(await evaluate(page, `typeof ${global}`)).toBe('undefined');
  }
  expect(await evaluate(page, 'typeof window.vibefaceClient')).toBe('object');
  expect(await evaluate(page, 'typeof window.vibefaceClient.desktop')).toBe('object');
  await screenshot(page, 'desktop-welcome');

  await desktop.invoke('chooseNodeMode', 'controller');
  const selected = await desktop.status();
  assert.equal(selected.nodeMode, 'controller');
  assert.equal(selected.core.kind, 'none');
  assert.equal((await desktop.inspect()).cores.length, 0);
  await desktop.invoke('focusClient');
  const connected = await desktop.inspect();
  const main = clientWindow(connected);
  assert(isClientUrl(main.url), 'the sole client window uses the trusted Vibeface origin');
  await expect.poll(() => evaluate(main, 'location.pathname'), { timeout: 30000 }).toBe('/connect');
  expect(await evaluate(main, 'Boolean(isSecureContext && crypto.subtle && indexedDB)')).toBe(true);
  await expect.poll(() => evaluate(main, 'document.querySelector("textarea[aria-label=\\"加入链接\\"]") !== null'),
    { timeout: 30000 }).toBe(true);
  const clientBridge = await evaluate(main, 'Object.keys(window.vibefaceClient).sort().join(",")');
  assert(typeof clientBridge === 'string');
  const bridgeKeys = clientBridge.split(',');
  assert.deepEqual(bridgeKeys, ['carrier', 'desktop', 'local', 'showNodePanel'],
    'the private client bridge contains only its purpose-specific controller operations');
  for (const global of ['require', 'process', 'module', 'Buffer', 'global', '__dirname']) {
    expect(await evaluate(main, `typeof ${global}`)).toBe('undefined');
  }
  await screenshot(main, 'desktop-client-welcome');
  assert.equal(connected.cores.length, 0, 'opening the client leaves Node startup explicit');
  await noPath(join(before.profile, 'node', 'identity.json'), 'controller selection does not create Node identity');
  await noPath(join(before.profile, 'node', 'config.json'), 'controller selection does not create Node configuration');
});

test('explicit local enable owns one bundled Node and restart keeps its identity', async ({ desktop }) => {
  const before = await desktop.inspect();
  await desktop.invoke('chooseNodeMode', 'local');
  assert.equal((await desktop.status()).core.kind, 'none');
  assert.equal((await desktop.inspect()).cores.length, 0, 'selecting local mode alone does not start a Node');

  await desktop.invoke('startOwned');
  const started = await desktop.inspect();
  assert.equal(clientWindow(started).webSocketDebuggerUrl, clientWindow(before).webSocketDebuggerUrl,
    'enabling and reconnecting the Node retains the same BrowserWindow');
  assert.equal(started.cores.length, 1, 'the explicit enable action starts one app-owned Core');
  assert.equal(started.status.core.kind, 'owned');
  const nodeId = started.status.core.deviceId;
  assert(typeof nodeId === 'string' && nodeId.length > 0);
  const dataDir = started.status.core.dataDir;
  const [actualDirectory, profileDirectory] = await Promise.all([
    stat(dataDir, { bigint: true }), stat(join(started.profile, 'node'), { bigint: true }),
  ]);
  assert.deepEqual([actualDirectory.dev, actualDirectory.ino], [profileDirectory.dev, profileDirectory.ino],
    'the owned Core uses exactly the App profile node directory');
  const epoch = JSON.parse(await readFile(join(dataDir, 'data-epoch.json'), 'utf8'));
  assert.equal(epoch.data_epoch, 1, 'Node data keeps the fixed epoch');
  const identity = await desktop.snapshotNode(dataDir);
  assert.equal(identity.deviceId, nodeId);
  assert.equal(identity.status, 'running', 'Core control observes the live app-owned Node');
  assert.equal(identity.version, started.runtime.core_version);
  assert.equal(identity.coreVersion, started.runtime.core_version);
  const panel = clientWindow(started);
  await openNodePanel(panel);
  await screenshot(panel, 'desktop-local-node-status');

  const closed = await desktop.closeWindows();
  assert.equal(closed.length, 1, 'the sole client window was closed to the tray');
  assert(closed.every(page => page.visibility === 'hidden'), 'the window close action hides the window into the tray');
  const tray = await desktop.inspect();
  assert.equal(tray.pid, started.pid, 'closing windows leaves the same desktop process in its tray lifecycle');
  assert.deepEqual(tray.cores, started.cores, 'closing windows keeps the app-owned child running');
  const closedWindowState = await desktop.status();
  assert.equal(closedWindowState.clientVisible, false);
  assert.equal(closedWindowState.core.kind, 'owned');
  assert.equal(closedWindowState.core.deviceId, nodeId);
  const stopped = await desktop.invoke('stopOwned');
  assert.equal(stopped.core.kind, 'offline');
  assert.equal((await desktop.inspect()).cores.length, 0, 'explicit stop ends the app-owned child');
  await desktop.invoke('startOwned');
  const restarted = await desktop.inspect();
  assert.equal(restarted.status.core.kind, 'owned');
  assert.equal(restarted.status.core.deviceId, nodeId, 'starting again preserves the original Node identity');
  assert.deepEqual(await desktop.snapshotNode(dataDir), identity);
  const exited = await desktop.quit();
  assert.equal(exited.forced, false, 'the app exited through its explicit menu quit action');
  assert.equal(exited.pid, restarted.pid);
  assert.equal(exited.owned.length, 1, 'quit reports only the Core owned by this app');
  assert.deepEqual(exited.surviving, [], 'explicit app quit stops its owned Core');
  const after = await desktop.start();
  assert.equal(after.status.core.kind, 'offline');
  assert.equal(after.status.core.deviceId, nodeId, 'the identity survives a full application restart');
  assert.equal(after.profile, before.profile);
});

test('local discovery chooses among healthy Nodes and explicitly starts a stopped Node unchanged', async ({ desktop }) => {
  const original = await desktop.inspect();
  await desktop.quit();
  const first = await desktop.createFixture(true);
  const second = await desktop.createFixture(true);
  assert.notEqual(first.peerPort, second.peerPort);
  assert.notEqual(first.localPort, second.localPort);
  assert.notEqual(first.deviceId, second.deviceId);
  const firstState = await desktop.snapshotNode(first.dataDir);
  const secondState = await desktop.snapshotNode(second.dataDir);
  assert.match(first.dataDir, /[^\x20-\x7e]/);
  assert.match(first.dataDir, / /);

  await desktop.start();
  await desktop.invoke('chooseNodeMode', 'controller');
  await desktop.invoke('discoverNodes');
  await persistWindowsDiscoveryCensus(desktop, [first.dataDir, second.dataDir]);
  const choice = await desktop.status();
  assert.equal(choice.core.kind, 'none', 'discovery never chooses arbitrarily among multiple healthy Nodes');
  assert.equal(choice.selectedNode, null);
  assert.equal(choice.discovery.kind, 'ready');
  const healthy = choice.discovery.nodes.filter(node => node.kind === 'running');
  assert.equal(healthy.length, 2, `the chooser presents both healthy fixture Nodes exactly once; warnings=${JSON.stringify(choice.discovery.warnings)}`);
  assert.deepEqual(new Set(healthy.map(node => node.deviceId)), new Set([first.deviceId, second.deviceId]));
  const chooser = clientWindow(await desktop.inspect());
  await openNodePanel(chooser);
  await expect.poll(() => connectButtons(chooser), { timeout: 30000 }).toBe(2);
  await screenshot(chooser, 'desktop-discovery-choices');
  const candidate = healthy.find(node => node.deviceId === first.deviceId);
  assert(candidate && typeof candidate.id === 'string');
  await desktop.invoke('connectDiscoveredNode', candidate.id);
  const attached = await desktop.status();
  assert.equal(attached.core.kind, 'none', 'selecting a client target does not change App child ownership');
  assert.equal(attached.selectedNode?.deviceId, first.deviceId);
  assert.equal(attached.selectedNode?.dataDir, first.dataDir, 'the selected Node is in its custom path outside the app profile');
  const attachedPage = clientWindow(await desktop.inspect());
  assert(isClientUrl(attachedPage.url), 'connecting retains the sole trusted client window');
  await screenshot(attachedPage, 'desktop-discovered-node-status');

  await openNodePanel(attachedPage);
  const selectedDocument = await evaluate(attachedPage, 'performance.timeOrigin');
  expect(await evaluate(attachedPage, `(() => {
    const clear = [...document.querySelectorAll('button')].find(button => button.textContent?.trim() === '清除');
    if (!clear || clear.disabled) return false;
    clear.click();
    return true;
  })()`)).toBe(true);
  await expect.poll(() => evaluate(attachedPage, 'performance.timeOrigin'), { timeout: 30000 }).not.toBe(selectedDocument);
  await expect.poll(() => evaluate(attachedPage, 'location.pathname'), { timeout: 30000 }).toBe('/settings/desktop-node');
  const cleared = await desktop.inspect();
  assert.equal(cleared.status.selectedNode, null, 'clearing selection also replaces the old client session');
  assert.equal(clientWindow(cleared).webSocketDebuggerUrl, attachedPage.webSocketDebuggerUrl,
    'clearing selection reloads the same window');
  assert.equal((await desktop.snapshotNode(first.dataDir)).status, 'running', 'clearing access leaves the Node running');
  await desktop.invoke('connectDiscoveredNode', candidate.id);

  await desktop.stopFixture(first.dataDir);
  await desktop.invoke('discoverNodes');
  const stoppedChoice = await desktop.status();
  assert.equal(stoppedChoice.discovery.kind, 'ready');
  const stoppedNode = stoppedChoice.discovery.nodes.find(node => node.deviceId === first.deviceId);
  assert.equal(stoppedNode?.kind, 'stopped', 'the existing fixture is identified as stopped');
  assert(stoppedNode);
  assert.equal(stoppedChoice.core.kind, 'none', 'rescanning never starts a child');
  assert.equal(stoppedChoice.selectedNode?.deviceId, second.deviceId, 'the sole remaining healthy Node is selected');
  const stoppedPanel = clientWindow(await desktop.inspect());
  await openNodePanel(stoppedPanel);
  await expect.poll(() => evaluate(stoppedPanel, '[...document.querySelectorAll("button")].filter((node) => node.textContent?.trim() === "启动并连接").length'), { timeout: 30000 }).toBe(1);
  await desktop.invoke('startDiscoveredNode', stoppedNode.id);
  const explicitlyStarted = await desktop.status();
  assert.equal(explicitlyStarted.core.kind, 'owned', 'the explicit start action owns only the child it starts');
  assert.equal(explicitlyStarted.core.deviceId, first.deviceId);
  assert.equal(explicitlyStarted.selectedNode?.deviceId, first.deviceId);
  assert.equal((await desktop.snapshotNode(first.dataDir)).deviceId, firstState.deviceId);
  assert.deepEqual(await desktop.snapshotNode(first.dataDir), firstState, 'explicit start changes neither Node identity nor config');
  assert.deepEqual(await desktop.snapshotNode(second.dataDir), secondState, 'connecting and starting another Node leaves the second untouched');

  const quit = await desktop.quit();
  assert.equal(quit.forced, false, 'the app exited through its explicit menu quit action');
  assert.equal(quit.owned.length, 1, 'only the Core explicitly started by the App is owned');
  assert.deepEqual(quit.surviving, [], 'the app has no owned Core to leave behind');
  for (const fixture of [first, second]) {
    const state = await desktop.snapshotNode(fixture.dataDir);
    assert.equal(state.deviceId, fixture.deviceId);
    assert.equal(state.status, fixture === first ? 'stopped' : 'running', 'quit stops its own child and leaves the other Core running');
  }
  await desktop.start();
  assert.equal((await desktop.inspect()).profile, original.profile);
});

test('one existing healthy Node is discovered and selected when the desktop opens', async ({ desktop }) => {
  await desktop.quit();
  const fixture = await desktop.createFixture(true);
  const before = await desktop.snapshotNode(fixture.dataDir);
  const opened = await desktop.start();
  await persistWindowsDiscoveryCensus(desktop, [fixture.dataDir]);
  assert.equal(opened.status.nodeMode, 'controller', `automatic discovery warnings=${opened.status.discovery.kind === 'ready' ? JSON.stringify(opened.status.discovery.warnings) : opened.status.discovery.kind}`);
  assert.deepEqual(opened.status.selectedNode, { dataDir: fixture.dataDir, deviceId: fixture.deviceId });
  assert.equal(opened.status.core.kind, 'none');
  assert.deepEqual(opened.cores, [], 'automatic connection starts no App-owned Core');
  const page = clientWindow(opened);
  assert(isClientUrl(page.url), 'the existing Node opens in the packaged client automatically');
  const discoveredId = await evaluate(page, '(async () => (await window.vibefaceClient.local.discoverLocalSetup())?.entry.device_id ?? null)()');
  assert.equal(discoveredId, fixture.deviceId, 'the native client reaches the selected existing Node');
  await desktop.quit();
  assert.deepEqual(await desktop.snapshotNode(fixture.dataDir), before, 'connecting and quitting preserve the existing running Node');
});

test('the optional background Node survives app quit and removal preserves its Node data', async ({ desktop }) => {
  const initial = await desktop.inspect();
  await desktop.invoke('chooseNodeMode', 'local');
  const installed = await desktop.invoke('installBackgroundNode');
  assert.equal(installed.backgroundNode.kind, 'installed');
  const nodeId = installed.backgroundNode.nodeId;
  assert.equal(installed.core.kind, 'none', 'a fresh GUI background install never pre-starts an App-owned Node');
  assert.deepEqual((await desktop.inspect()).cores, []);
  const installedData = installed.backgroundNode.dataDir;
  const data = await desktop.snapshotNode(installedData);
  const service = await desktop.serviceStatus(nodeId);
  assert.equal(service.state, 'installed');
  assert.equal(service.device_id, nodeId);
  assert.equal(service.data_dir, installedData);
  assert.equal(service.service_running, true);
  assert.equal(service.core_status, 'running', 'installation waits for the actual Core to be healthy');
  const serviceId = service.service_id;
  assert(typeof serviceId === 'string' && serviceId.length > 0);
  assert(typeof service.managed_root === 'string');
  const managedPackage = await assertManagedPackageUnderReleases(service.managed_root);
  const managedIdentity = await stat(managedPackage, { bigint: true });
  const sourceRuntime = process.env.VIBEFACE_E2E_DESKTOP_RUNTIME ?? '';
  for (const binary of ['BUILD-INFO.json', process.platform === 'win32' ? 'vibeface-core.exe' : 'vibeface-core']) {
    const sourceHash = createHash('sha256').update(await readFile(join(sourceRuntime, binary))).digest('hex');
    const managedHash = createHash('sha256').update(await readFile(join(managedPackage, binary))).digest('hex');
    assert.equal(managedHash, sourceHash, `the durable service runtime preserves the verified ${binary} bytes`);
  }

  await desktop.invoke('stopBackgroundNode');
  const beforeRecovery = await desktop.snapshotNode(installedData);
  const canonicalPackage = process.platform === 'win32' ? toNamespacedPath(managedPackage) : managedPackage;
  const pending = join(installedData, 'update-pending.json');
  await writeFile(pending, JSON.stringify({
    phase: 'prepared', previous: canonicalPackage, candidate: canonicalPackage, device_id: nodeId,
    backup: join(service.managed_root, 'backups', 'e2e-interrupted'), resume_service: true,
  }), { mode: 0o600 });
  const pendingBytes = await readFile(pending, 'utf8');
  const recoveryObserved = await desktop.invoke('discoverNodes');
  assert.equal(recoveryObserved.backgroundNode.kind, 'installed');
  assert.equal(recoveryObserved.backgroundNode.runtimeUpdate.recoveryRequired, true);
  await assert.rejects(desktop.invoke('startBackgroundNode'), /继续更新/);
  await assert.rejects(desktop.invoke('removeBackgroundNode'), /继续更新/);
  assert.equal((await desktop.serviceStatus(nodeId)).service_running, false, 'pending controls never start the unconfirmed package');
  assert.equal(await readFile(pending, 'utf8'), pendingBytes, 'observation and refusals retain the interrupted operation');
  const repaired = await desktop.invoke('updateBackgroundNode');
  assert.equal(repaired.backgroundNode.kind, 'installed');
  assert.equal(repaired.backgroundNode.health, 'stopped', 'recovery respects the explicit stopped state');
  assert.equal(repaired.backgroundNode.runtimeUpdate.recoveryRequired, false);
  await noPath(pending, 'the confirmed stopped recovery clears only its update journal');
  const afterRecovery = await desktop.snapshotNode(installedData);
  assert.equal(afterRecovery.status, 'stopped');
  assert.equal(afterRecovery.deviceId, nodeId);
  assert.deepEqual(afterRecovery.values, beforeRecovery.values, 'recovery preserves identity, configuration and update policy');
  await desktop.invoke('startBackgroundNode');
  assert.equal((await desktop.serviceStatus(nodeId)).core_status, 'running');

  const firstRemoval = await desktop.invoke('removeBackgroundNode');
  assert.equal(firstRemoval.backgroundNode.kind, 'absent');
  assert.equal((await desktop.snapshotNode(installedData)).status, 'stopped');
  await desktop.invoke('startOwned');
  const temporary = await desktop.inspect();
  assert.equal(temporary.status.core.kind, 'owned');
  assert.equal(temporary.status.core.deviceId, nodeId);
  const handedOff = await desktop.invoke('installBackgroundNode');
  assert.equal(handedOff.backgroundNode.kind, 'installed');
  assert.equal(handedOff.backgroundNode.nodeId, nodeId, 'service handoff preserves the App-owned Node identity');
  assert.deepEqual((await desktop.inspect()).cores, [], 'the App-owned child exits before the same Node is installed as a service');
  assert.equal((await desktop.serviceStatus(nodeId)).core_status, 'running');

  const exited = await desktop.quit();
  assert.equal(exited.forced, false, 'the app exited through its explicit menu quit action');
  assert.deepEqual(exited.owned, [], 'the background Core is not an application-owned child');
  assert.deepEqual(exited.surviving, [], 'no app-owned Core survives its owning app');
  const afterQuit = await desktop.serviceStatus(nodeId);
  assert.equal(afterQuit.state, 'installed', 'the OS current-user manager still reports the service installed and running');
  assert.equal(afterQuit.service_id, serviceId);
  assert.equal(afterQuit.service_running, true);
  assert.equal(afterQuit.managed_root, service.managed_root);
  if (process.platform === 'linux') {
    const detached = await desktop.detachSourceRuntime(nodeId);
    assert.equal(detached.state, 'installed');
    assert.equal(detached.service_id, serviceId);
    assert.equal(detached.core_status, 'running', 'the copied Core keeps its same identity healthy while the AppImage runtime is unavailable');
  }
  const afterQuitNode = await desktop.snapshotNode(installedData);
  assert.equal(afterQuitNode.status, 'running');
  assert.equal(afterQuitNode.deviceId, data.deviceId);
  assert.deepEqual(afterQuitNode.values, data.values, 'the same Node identity/config remains durable after app quit');
  assert(typeof afterQuit.managed_root === 'string');
  const afterQuitIdentity = await stat(await assertManagedPackageUnderReleases(afterQuit.managed_root), { bigint: true });
  assert.deepEqual(
    [afterQuitIdentity.dev, afterQuitIdentity.ino], [managedIdentity.dev, managedIdentity.ino],
    'the native registration still points to the same durable managed runtime after app quit',
  );

  const relaunched = await desktop.start();
  const observed = await desktop.status();
  assert.equal(observed.backgroundNode.kind, 'installed');
  assert.equal(observed.backgroundNode.nodeId, nodeId);
  assert.equal(observed.backgroundNode.health, 'running');
  assert.equal((await desktop.serviceStatus(nodeId)).service_id, serviceId, 'relaunch reads the same native registration');
  const removed = await desktop.invoke('removeBackgroundNode');
  assert.equal(removed.backgroundNode.kind, 'absent');
  const afterRemovalNode = await desktop.snapshotNode(installedData);
  assert.equal(afterRemovalNode.status, 'stopped');
  assert.equal(afterRemovalNode.deviceId, data.deviceId);
  assert.deepEqual(afterRemovalNode.values, data.values, 'service removal retains identity and config');
  const uninstalled = await desktop.serviceStatus(nodeId);
  assert.equal(uninstalled.state, 'not_installed', 'the exact task-owned registration was removed');
  assert.equal(uninstalled.service_running, null);
  assert.equal(relaunched.profile, initial.profile);
});
