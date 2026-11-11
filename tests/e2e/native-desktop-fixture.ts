import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { isAbsolute, relative, resolve, sep } from 'node:path';
import { promisify } from 'node:util';

const execute = promisify(execFile);
const root = resolve(import.meta.dirname, '../..');

function record(value: unknown): Record<string, unknown> {
  assert(value !== null && typeof value === 'object' && !Array.isArray(value), 'Native acceptance evidence must be an object');
  return Object.fromEntries(Object.entries(value));
}

export async function nativeDesktopAcceptance() {
  const application = process.env.VIBEFACE_E2E_DESKTOP_APP;
  const runtime = process.env.VIBEFACE_E2E_DESKTOP_RUNTIME;
  assert(application && isAbsolute(application), 'Name the built native Desktop application with VIBEFACE_E2E_DESKTOP_APP');
  assert(runtime && isAbsolute(runtime), 'Name the same-source sealed runtime with VIBEFACE_E2E_DESKTOP_RUNTIME');
  const artifacts = resolve(process.env.VIBEFACE_E2E_DESKTOP_OUTPUT ?? resolve(root, '.artifacts/native-desktop-acceptance'));
  const location = relative(resolve(root, '.artifacts'), artifacts);
  assert(location && location.split(sep)[0] !== '..' && !isAbsolute(location), 'Native evidence stays under this task workspace artifacts');
  await mkdir(artifacts, { recursive: true });
  const output = await mkdtemp(resolve(artifacts, 'window-'));
  try {
    const result = await execute(process.execPath, [resolve(root, 'scripts/e2e-desktop.mjs'), '--app', application, '--runtime', runtime, '--output', output],
      { cwd: root, env: process.env, timeout: 100000, killSignal: 'SIGTERM', maxBuffer: 1024 * 1024 });
    await writeFile(resolve(output, 'driver.log'), result.stdout + result.stderr);
  } catch (failure) {
    const message = failure instanceof Error ? failure.message : String(failure);
    await writeFile(resolve(output, 'driver-failure.log'), message);
    throw new Error(`Native OS window acceptance failed; evidence: ${relative(root, output)}`, { cause: failure });
  }
  const evidence = record(JSON.parse(await readFile(resolve(output, 'native-window.json'), 'utf8')));
  const window = record(evidence.window);
  const operatingSystem = record(evidence.windowEvidence);
  const exited = record(evidence.exit);
  assert.equal(window.runtime_ui, 'gpui');
  assert.equal(window.phase, 'completed');
  assert.equal(window.node_started, false);
  assert.equal(window.input_text, 'native-gpui-fixture');
  assert.equal(window.focused, true);
  assert(typeof window.action_count === 'number' && window.action_count >= 1);
  assert(typeof window.pid === 'number' && window.pid > 0);
  assert.equal(operatingSystem.pid, window.pid, 'The OS captured the actual native application process');
  assert(operatingSystem.window_id, 'The OS identified a window owned by this process');
  assert(['System Events', 'SendKeys', 'X11 native events'].includes(String(operatingSystem.keyboard)));
  assert.equal(operatingSystem.screenshot, 'native-window.png');
  const png = await readFile(resolve(output, 'native-window.png'));
  assert(png.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])));
  assert.equal(png.readUInt32BE(16), operatingSystem.width);
  assert.equal(png.readUInt32BE(20), operatingSystem.height);
  assert.deepEqual(exited, { code: 0, signal: null });
  return relative(root, output);
}
