import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { expect } from 'e2e';
import { evaluate, screenshot, test, type Inspection, type Page } from '../desktop';

const ELECTRON_VERSION = '45.2.3';
const CORE_ORIGIN = 'http://127.0.0.1:4791';
const BUILD_INFO_FIELDS = ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
  'core_version', 'core_source_commit', 'app_version', 'app_source_commit'];

function windows(current: Inspection) {
  const pages = current.pages.filter(entry => entry.type === 'page');
  const business = pages.filter(entry => new URL(entry.url).origin === CORE_ORIGIN);
  const launcher = pages.filter(entry => new URL(entry.url).origin !== CORE_ORIGIN);
  assert.equal(business.length, 1, 'exactly one business window loads the Core origin');
  assert.equal(launcher.length, 1, 'exactly one trusted launcher window stands beside it');
  return { business: business[0] as Page, launcher: launcher[0] as Page };
}

async function text(target: Page, expression: string) {
  const value = await evaluate(target, expression);
  assert(typeof value === 'string', `${expression} reads as text`);
  return value;
}

test('the packaged application runs its own sealed runtime and the Core it owns', async ({ desktop }) => {
  const current = await desktop.inspect();
  const runtime = JSON.parse(await readFile(join(process.env.VIBEFACE_E2E_DESKTOP_RUNTIME ?? '', 'BUILD-INFO.json'), 'utf8'));
  assert.deepEqual(Object.keys(current.runtime).sort(), [...BUILD_INFO_FIELDS].sort());
  assert.deepEqual(current.runtime, runtime, 'the bundled runtime is the verified package, byte for byte');
  assert.equal(current.cores.length, 1, 'the application owns exactly one Core process');
  assert.match(current.profile, /[^\x20-\x7e]/, 'the profile path carries characters outside ASCII');
  assert.match(current.profile, / /, 'the profile path carries a space');

  const { business, launcher } = windows(current);
  const response = await fetch(`${CORE_ORIGIN}/api/info`, { signal: AbortSignal.timeout(30000) });
  assert(response.ok, `the owned Core answered ${response.status}`);
  const info: unknown = await response.json();
  assert(info !== null && typeof info === 'object' && 'version' in info);
  assert.equal(info.version, current.runtime.core_version, 'the serving Core is the bundled one');

  for (const [name, target] of Object.entries({ business, launcher })) {
    const agent = await text(target, 'navigator.userAgent');
    assert.match(agent, new RegExp(`Electron/${ELECTRON_VERSION.replace(/\./g, '\\.')}`), `the ${name} window is the built Electron application`);
    expect(await evaluate(target, 'window.innerWidth > 0 && window.innerHeight > 0')).toBe(true);
    await screenshot(target, `desktop-${name}-window`);
  }
});

test('no desktop window reaches Node or a privileged bridge', async ({ desktop }) => {
  const { business, launcher } = windows(await desktop.inspect());
  for (const [name, target] of Object.entries({ business, launcher })) {
    for (const global of ['require', 'process', 'module', 'Buffer', 'global', '__dirname']) {
      expect(await evaluate(target, `typeof ${global}`)).toBe('undefined');
    }
    assert.equal(await evaluate(target, `Object.keys(window).filter(key => /electron|node|runtime|spawn|exec/i.test(key)).join(',')`),
      '', `the ${name} window exposes no process or runtime bridge`);
  }
  // The business window is sandboxed, so it carries no preload bridge of its own at all.
  assert.equal(await evaluate(business, 'Object.keys(window).filter(key => key.startsWith("vibeface")).length'), 0);
});

test('quitting stops only the Core it owns and a restart serves the window again', async ({ desktop }) => {
  const before = await desktop.inspect();
  const stopped = await desktop.quit();
  assert.equal(stopped.pid, before.pid, 'the application that quit is the one that ran');
  assert.deepEqual(stopped.owned, before.cores, 'the Core it owned is the one it started');
  assert.deepEqual(stopped.surviving, [], 'quitting leaves no owned Core behind');

  const after = await desktop.start();
  assert.notEqual(after.pid, before.pid, 'the restart runs a new application process');
  assert.equal(after.cores.length, 1, 'the restart owns exactly one Core again');
  assert.notDeepEqual(after.cores, before.cores, 'the restart starts a new Core rather than adopting the old one');
  assert.equal(after.profile, before.profile, 'the restart keeps the same isolated profile');
  assert.deepEqual(after.runtime, before.runtime, 'the restart runs the same sealed runtime');

  const { business } = windows(after);
  expect(await text(business, 'document.location.origin')).toBe(CORE_ORIGIN);
  await screenshot(business, 'desktop-business-window-restarted');
});
