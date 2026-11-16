import { randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { mobile } from '@e2e-dev/mobile';
import { nativeIosConfig, nativeIosInputs } from './native-ios-config';

const artifacts = resolve(import.meta.dirname, '../../.artifacts/native/workspace');
const results = resolve(import.meta.dirname, '.artifacts/workspace-native');
const inputs = nativeIosInputs({ taskRoot: artifacts, invitation: resolve(artifacts, 'invitation.txt'), env: process.env });
process.env.AGENT_DEVICE_HOME ??= inputs.agentDeviceHome;
process.env.AGENT_DEVICE_STATE_DIR ??= inputs.agentDeviceStateDir;

export default nativeIosConfig({
  driver: mobile,
  app: inputs.app,
  simulator: inputs.simulator,
  target: { name: 'workspace-ios', session: 'workspace-native-acceptance' },
  tests: ['workspace.native.ts'],
  output: results,
  cache: { dir: resolve(results, 'cache') },
  timeout: 600000,
  secrets: { nativeInvitation: async () => (await readFile(inputs.invitation, 'utf8')).trim(), nativeWebhookSecret: randomBytes(24).toString('base64url') },
});
