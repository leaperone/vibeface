import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { mobile } from '@e2e-dev/mobile';
import { nativeIosConfig, nativeIosInputs } from './native-ios-config';

const artifacts = resolve(import.meta.dirname, '../../.artifacts/native');
const runnerArtifacts = resolve(import.meta.dirname, '.artifacts/native');
const inputs = nativeIosInputs({ taskRoot: artifacts, invitation: resolve(artifacts, 'network/invitation.txt'), env: process.env });
process.env.AGENT_DEVICE_HOME ??= inputs.agentDeviceHome;
process.env.AGENT_DEVICE_STATE_DIR ??= inputs.agentDeviceStateDir;

export default nativeIosConfig({
  driver: mobile,
  app: inputs.app,
  simulator: inputs.simulator,
  target: { name: 'vdp-ios', session: 'vdp-native-acceptance' },
  tests: ['native-transfers.e2e.ts'],
  output: resolve(runnerArtifacts, 'e2e'),
  cache: { dir: resolve(runnerArtifacts, 'cache') },
  timeout: 240000,
  secrets: { nativeInvitation: async () => (await readFile(inputs.invitation, 'utf8')).trim() },
});
