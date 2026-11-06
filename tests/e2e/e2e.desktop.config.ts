import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import type { E2EConfig } from 'e2e';

const localArtifacts = resolve(import.meta.dirname, '.artifacts/desktop');
assert(process.env.VIBEFACE_E2E_DESKTOP_APP, 'Name the built desktop application assigned to this acceptance run');
assert(process.env.VIBEFACE_E2E_DESKTOP_RUNTIME, 'Name the verified runtime the desktop application was built around');

// Each test launches the real application through scripts/e2e-desktop.mjs and drives the Electron
// windows it already opened over the application's own DevTools endpoint, so nothing here is a
// browser beside the application.
export default {
  tests: ['desktop-shell.e2e.ts'],
  targets: [{ name: 'vibeface-desktop', platform: process.platform === 'win32' ? 'windows' : 'macos' }],
  workers: 1,
  retries: 0,
  timeout: 300000,
  output: localArtifacts,
  cache: { dir: resolve(localArtifacts, 'cache') },
} satisfies E2EConfig;
