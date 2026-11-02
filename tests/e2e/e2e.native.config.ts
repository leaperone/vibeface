import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import type { E2EConfig } from 'e2e';
import { mobile } from '@e2e-dev/mobile';

const root = resolve(import.meta.dirname, '../..');
const app = process.env.VIBEFACE_E2E_IOS_APP;
const simulator = process.env.VIBEFACE_E2E_SIMULATOR;
assert(app && simulator && /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/i.test(simulator), 'Explicitly name the build and dedicated simulator assigned to this acceptance task');
process.env.AGENT_DEVICE_HOME ??= resolve(root, '.artifacts/native/agent-device');
process.env.AGENT_DEVICE_STATE_DIR ??= resolve(root, '.artifacts/native/agent-device');
for (const state of [process.env.AGENT_DEVICE_HOME, process.env.AGENT_DEVICE_STATE_DIR]) {
  const location = relative(resolve(root, '.artifacts/native'), resolve(state));
  assert(location && !location.startsWith('..') && !location.startsWith('/'), 'Native driver state remains in this task');
}
const invitation = process.env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? resolve(root, '.artifacts/native/network/invitation.txt');
const inside = relative(resolve(root, '.artifacts/native'), resolve(invitation));
assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Native invitation remains in this task');

export default {
  tests: ['tests/e2e/native-transfers.e2e.ts'],
  targets: [{ name: 'vdp-ios', engine: mobile({ platform: 'ios', device: simulator, session: 'vdp-native-acceptance' }),
    app: { appPath: resolve(app), bundleId: 'one.leaper.vibeface' } }],
  workers: 1,
  retries: 0,
  trace: 'off',
  video: 'off',
  cleanupTimeout: 90000,
  timeout: 240000,
  launchTimeout: 240000,
  output: resolve(root, '.artifacts/native/e2e'),
  cache: { dir: resolve(root, '.artifacts/native/cache') },
  secrets: { nativeInvitation: async () => (await readFile(invitation, 'utf8')).trim() },
} satisfies E2EConfig;
