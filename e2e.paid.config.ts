import assert from 'node:assert/strict';
import { resolve, relative } from 'node:path';
import type { E2EConfig } from 'e2e';
import { mobile } from '@e2e-dev/mobile';
import { artifacts, deadline, facts, insideArtifacts, invitation, root } from './tests/paid-delivery-control';

const apk = process.env.VIBEFACE_E2E_ANDROID_APK;
const serial = process.env.VIBEFACE_E2E_ANDROID_SERIAL;
assert(apk && serial && /^emulator-[0-9]+$/.test(serial), 'Explicitly name the same-source release APK and dedicated task AVD');
insideArtifacts(apk);
process.env.AGENT_DEVICE_HOME ??= resolve(artifacts, 'agent-device');
process.env.AGENT_DEVICE_STATE_DIR ??= resolve(artifacts, 'agent-device');
for (const state of [process.env.AGENT_DEVICE_HOME, process.env.AGENT_DEVICE_STATE_DIR]) {
  const suffix = relative(artifacts, resolve(state));
  assert(suffix && !suffix.startsWith('..') && !suffix.startsWith('/'), 'Paid native driver state remains in this task');
}
process.env.PLAYWRIGHT_BROWSERS_PATH ??= resolve(root, '.artifacts/e2e/browsers');
const { web } = await import('@e2e-dev/web');
const environment = await facts();

export default {
  tests: ['tests/paid-web-transfers.paid.ts', 'tests/paid-native-transfers.paid.ts'],
  targets: [
    { name: 'vdp-paid-web', engine: web(), app: { url: environment.nodes.web.url } },
    { name: 'vdp-paid-android', engine: mobile({ platform: 'android', device: serial, session: 'vdp-paid-acceptance' }),
      app: { appPath: resolve(apk), bundleId: 'one.leaper.vibeface' } },
  ],
  workers: 2,
  retries: 0,
  video: 'off',
  timeout: deadline(),
  launchTimeout: 240000,
  output: resolve(artifacts, 'e2e'),
  cache: { dir: resolve(artifacts, 'cache') },
  secrets: { paidWebInvitation: () => invitation('web'), paidAndroidInvitation: () => invitation('android') },
} satisfies E2EConfig;
