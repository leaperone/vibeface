import assert from 'node:assert/strict';
import { insideTaskArtifacts } from '../support/artifact-paths';
import { resolve } from 'node:path';
import type { mobile } from '@e2e-dev/mobile';
import type { E2EConfig } from 'e2e';

export function nativeIosInputs({ taskRoot, invitation, env }: { taskRoot: string; invitation: string; env: NodeJS.ProcessEnv }) {
  const app = env.VIBEFACE_E2E_IOS_APP;
  const simulator = env.VIBEFACE_E2E_SIMULATOR;
  assert(app && simulator && /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/i.test(simulator), 'Explicitly name the build and dedicated simulator assigned to this acceptance task');
  const agentDeviceHome = env.AGENT_DEVICE_HOME ?? resolve(taskRoot, 'agent-device');
  const agentDeviceStateDir = env.AGENT_DEVICE_STATE_DIR ?? resolve(taskRoot, 'agent-device');
  for (const state of [agentDeviceHome, agentDeviceStateDir]) {
    assert(insideTaskArtifacts(taskRoot, resolve(state), { allowRoot: false }), 'Native driver state remains in this task');
  }
  const invitationFile = env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? invitation;
  assert(insideTaskArtifacts(taskRoot, resolve(invitationFile), { allowRoot: false }), 'Native invitation remains in this task');
  return { app: resolve(app), simulator, agentDeviceHome, agentDeviceStateDir, invitation: invitationFile };
}

export function nativeIosConfig({ driver, app, simulator, target, tests, output, cache, timeout, secrets }: {
  driver: typeof mobile;
  app: string;
  simulator: string;
  target: { name: string; session: string };
} & Required<Pick<E2EConfig, 'tests' | 'output' | 'cache' | 'timeout' | 'secrets'>>) {
  return {
    tests,
    targets: [{ name: target.name, engine: driver({ platform: 'ios', device: simulator, session: target.session, transition: 1000 }),
      app: { appPath: app, bundleId: 'one.leaper.vibeface' } }],
    workers: 1,
    retries: 0,
    trace: 'off',
    video: 'off',
    cleanupTimeout: 90000,
    timeout,
    launchTimeout: 240000,
    output,
    cache,
    secrets,
  } satisfies E2EConfig;
}
