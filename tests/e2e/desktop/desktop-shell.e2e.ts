import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { access, readFile } from 'node:fs/promises';
import { join, sep } from 'node:path';
import { realpath } from 'node:fs/promises';
import { expect } from 'e2e';
import { evaluate, screenshot, test, type Inspection, type Page } from '../desktop';

const ELECTRON_VERSION = '45.2.3';
const BUILD_INFO_FIELDS = ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
  'core_version', 'core_source_commit', 'app_version', 'app_source_commit'];

function isClientUrl(raw: string): boolean {
  const url = new URL(raw);
  return url.protocol === 'vibeface-app:' && url.host === 'app';
}

function windows(current: Inspection) {
  const pages = current.pages.filter(entry => entry.type === 'page');
  const client = pages.filter(entry => isClientUrl(entry.url));
  const launcher = pages.filter(entry => entry.url.startsWith('vibeface-launcher://app/'));
  assert.equal(launcher.length, 1, 'exactly one trusted launcher window exists');
  const launcherPage = launcher[0];
  assert(launcherPage);
  return { client: client[0], launcher: launcherPage };
}

async function text(target: Page, expression: string) {
  const value = await evaluate(target, expression);
  assert(typeof value === 'string', `${expression} reads as text`);
  return value;
}

async function noPath(path: string, message: string) {
  await assert.rejects(access(path), { code: 'ENOENT' }, message);
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
  assert.equal(before.status.nodeMode, 'undecided');
  await noPath(join(before.profile, 'node'), 'a fresh controller profile has no Node data directory');

  const { launcher } = windows(before);
  const agent = await text(launcher, 'navigator.userAgent');
  assert.match(agent, new RegExp(`Electron/${ELECTRON_VERSION.replace(/\./g, '\\.')}`));
  for (const global of ['require', 'process', 'module', 'Buffer', 'global', '__dirname']) {
    expect(await evaluate(launcher, `typeof ${global}`)).toBe('undefined');
  }
  expect(await evaluate(launcher, 'typeof window.vibefaceLauncher')).toBe('object');
  await screenshot(launcher, 'desktop-welcome');

  await desktop.invoke('chooseNodeMode', 'controller');
  const selected = await desktop.status();
  assert.equal(selected.nodeMode, 'controller');
  assert.equal(selected.core.kind, 'none');
  assert.equal((await desktop.inspect()).cores.length, 0);
  await desktop.invoke('openBusiness');
  const connected = await desktop.inspect();
  const client = windows(connected).client;
  assert(client, 'the controller opens its packaged private client page');
  assert(isClientUrl(client.url));
  expect(await evaluate(client, 'typeof window.vibefaceClient')).toBe('object');
  const clientBridge = await evaluate(client, 'Object.keys(window.vibefaceClient).sort().join(",")');
  assert(typeof clientBridge === 'string');
  const bridgeKeys = clientBridge.split(',');
  assert.deepEqual(bridgeKeys, ['carrier', 'local', 'showNodePanel'],
    'the private client bridge contains only its purpose-specific controller operations');
  for (const global of ['require', 'process', 'module', 'Buffer', 'global', '__dirname']) {
    expect(await evaluate(client, `typeof ${global}`)).toBe('undefined');
  }
  await screenshot(client, 'desktop-client-welcome');
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
  assert.equal(started.cores.length, 1, 'the explicit enable action starts one app-owned Core');
  assert.equal(started.status.core.kind, 'owned');
  const nodeId = started.status.core.deviceId;
  assert(typeof nodeId === 'string' && nodeId.length > 0);
  const dataDir = started.status.core.dataDir;
  assert.equal(dataDir, join(started.profile, 'node'));
  const epoch = JSON.parse(await readFile(join(dataDir, 'data-epoch.json'), 'utf8'));
  assert.equal(epoch.data_epoch, 1, 'Node data keeps the fixed epoch');
  const identity = await desktop.snapshotNode(dataDir);
  assert.equal(identity.deviceId, nodeId);
  assert.equal(identity.status, 'running', 'Core control observes the live app-owned Node');
  assert.equal(identity.version, started.runtime.core_version);
  assert.equal(identity.coreVersion, started.runtime.core_version);
  await desktop.invoke('openBusiness');
  await screenshot(windows(started).launcher, 'desktop-local-node-status');

  const closed = await desktop.closeWindows();
  assert.equal(closed.length, 2, 'the real launcher and client windows were closed');
  assert(closed.every(page => page.visibility === 'hidden'), 'the window close action hides the windows into the tray');
  const tray = await desktop.inspect();
  assert.equal(tray.pid, started.pid, 'closing windows leaves the same desktop process in its tray lifecycle');
  assert.deepEqual(tray.cores, started.cores, 'closing windows keeps the app-owned child running');
  const closedWindowState = await desktop.status();
  assert.equal(closedWindowState.businessOpen, false);
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
  const choice = await desktop.status();
  assert.equal(choice.core.kind, 'none', 'discovery never chooses arbitrarily among multiple healthy Nodes');
  assert.equal(choice.selectedNode, null);
  assert.equal(choice.discovery.kind, 'ready');
  const healthy = choice.discovery.nodes.filter(node => node.kind === 'running');
  assert.equal(healthy.length, 2, 'the chooser presents both healthy fixture Nodes exactly once');
  assert.deepEqual(new Set(healthy.map(node => node.deviceId)), new Set([first.deviceId, second.deviceId]));
  const chooser = windows(await desktop.inspect()).launcher;
  expect(await evaluate(chooser, 'document.querySelectorAll("#nodes li").length')).toBe(2);
  await screenshot(chooser, 'desktop-discovery-choices');
  const candidate = healthy.find(node => node.deviceId === first.deviceId);
  assert(candidate && typeof candidate.id === 'string');
  await desktop.invoke('connectDiscoveredNode', candidate.id);
  const attached = await desktop.status();
  assert.equal(attached.core.kind, 'none', 'selecting a client target does not change App child ownership');
  assert.equal(attached.selectedNode?.deviceId, first.deviceId);
  assert.equal(attached.selectedNode?.dataDir, first.dataDir, 'the selected Node is in its custom path outside the app profile');
  const attachedWindows = windows(await desktop.inspect());
  assert(attachedWindows.client);
  await screenshot(attachedWindows.client, 'desktop-discovered-node-status');

  await desktop.stopFixture(first.dataDir);
  await desktop.invoke('discoverNodes');
  const stoppedChoice = await desktop.status();
  assert.equal(stoppedChoice.discovery.kind, 'ready');
  const stoppedNode = stoppedChoice.discovery.nodes.find(node => node.deviceId === first.deviceId);
  assert.equal(stoppedNode?.kind, 'stopped', 'the existing fixture is identified as stopped');
  assert(stoppedNode);
  assert.equal(stoppedChoice.core.kind, 'none', 'rescanning never starts a child');
  assert.equal(stoppedChoice.selectedNode?.deviceId, second.deviceId, 'the sole remaining healthy Node is selected');
  const stoppedLauncher = windows(await desktop.inspect()).launcher;
  expect(await evaluate(stoppedLauncher, 'document.querySelectorAll("#nodes button[data-action=\\"start-discovered\\"]").length')).toBe(1);
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
  assert.equal(opened.status.nodeMode, 'controller');
  assert.deepEqual(opened.status.selectedNode, { dataDir: fixture.dataDir, deviceId: fixture.deviceId });
  assert.equal(opened.status.core.kind, 'none');
  assert.deepEqual(opened.cores, [], 'automatic connection starts no App-owned Core');
  const client = windows(opened).client;
  assert(client, 'the existing Node opens in the packaged client automatically');
  const discoveredId = await evaluate(client, '(async () => (await window.vibefaceClient.local.discoverLocalSetup())?.entry.device_id ?? null)()');
  assert.equal(discoveredId, fixture.deviceId, 'the native client reaches the selected existing Node');
  await desktop.quit();
  assert.deepEqual(await desktop.snapshotNode(fixture.dataDir), before, 'connecting and quitting preserve the existing running Node');
});

test('the optional background Node survives app quit and removal preserves its Node data', async ({ desktop }) => {
  const initial = await desktop.inspect();
  await desktop.invoke('chooseNodeMode', 'local');
  await desktop.invoke('startOwned');
  const provisioned = await desktop.status();
  assert.equal(provisioned.core.kind, 'owned');
  const nodeId = provisioned.core.deviceId;
  assert(typeof nodeId === 'string' && nodeId.length > 0);
  await desktop.invoke('stopOwned');
  const installed = await desktop.invoke('installBackgroundNode');
  assert.equal(installed.backgroundNode.kind, 'installed');
  assert.equal(installed.backgroundNode.nodeId, nodeId);
  const installedData = installed.backgroundNode.dataDir;
  const data = await desktop.snapshotNode(installedData);
  const service = await desktop.serviceStatus(nodeId);
  assert.equal(service.state, 'installed');
  assert.equal(service.device_id, nodeId);
  assert.equal(service.data_dir, installedData);
  assert.equal(service.service_running, true);
  const serviceId = service.service_id;
  assert(typeof serviceId === 'string' && serviceId.length > 0);
  assert(typeof service.managed_root === 'string');
  const managedPackage = await realpath(join(service.managed_root, 'current'));
  assert(managedPackage.startsWith(join(service.managed_root, 'releases') + sep),
    'the registered service executes a versioned copy under its profile-managed runtime root');
  const sourceRuntime = process.env.VIBEFACE_E2E_DESKTOP_RUNTIME ?? '';
  for (const binary of ['BUILD-INFO.json', process.platform === 'win32' ? 'vibeface-core.exe' : 'vibeface-core']) {
    const sourceHash = createHash('sha256').update(await readFile(join(sourceRuntime, binary))).digest('hex');
    const managedHash = createHash('sha256').update(await readFile(join(managedPackage, binary))).digest('hex');
    assert.equal(managedHash, sourceHash, `the durable service runtime preserves the verified ${binary} bytes`);
  }

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
  assert.equal(await realpath(join(afterQuit.managed_root as string, 'current')), managedPackage,
    'the native registration still points to the same durable managed runtime after app quit');

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
