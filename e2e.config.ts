import type { E2EConfig } from 'e2e';
import { randomBytes } from 'node:crypto';
import { resolve } from 'node:path';

process.env.PLAYWRIGHT_BROWSERS_PATH ??= resolve('.artifacts/e2e/browsers');
const { web } = await import('@e2e-dev/web');

export default {
  targets: [{
    name: 'chromium',
    engine: web(),
    app: {
      url: 'http://127.0.0.1:0',
      readyUrl: 'http://127.0.0.1:{port}/api/info',
      command: {
        executable: 'node',
        args: ['scripts/e2e-node.mjs', '{port}'],
        env: { VIBEFACE_E2E_BINARY: process.env.VIBEFACE_E2E_BINARY ?? 'vibeface' },
        log: '.artifacts/e2e/node.log',
      },
    },
  }],
  // shortcut: one fresh Node per run; add per-test fixtures when adding stateful flows.
  workers: 1,
  retries: 0,
  secrets: { recoveryPassword: randomBytes(24).toString('base64url') },
  output: '.artifacts/e2e/results',
  cache: { dir: '.artifacts/e2e/cache' },
} satisfies E2EConfig;
