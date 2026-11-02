import type { E2EConfig } from 'e2e';
import { randomBytes } from 'node:crypto';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '../..')
process.env.PLAYWRIGHT_BROWSERS_PATH ??= resolve(root, '.artifacts/e2e/browsers');
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
        args: [resolve(root, 'scripts/e2e-node.mjs'), '{port}'],
        env: { VIBEFACE_E2E_BINARY: process.env.VIBEFACE_E2E_BINARY ?? 'vibeface', VIBEFACE_E2E_CRYPTO_MODULE: process.env.VIBEFACE_E2E_CRYPTO_MODULE ?? '', VIBEFACE_E2E_APP_DIR: process.env.VIBEFACE_E2E_APP_DIR ?? '', VIBEFACE_E2E_PROFILE_FILE: process.env.VIBEFACE_E2E_PROFILE_FILE ?? '' },
        shutdownTimeout: 30000,
        log: resolve(root, '.artifacts/e2e/node.log'),
      },
    },
  }],
  workers: 1,
  retries: 0,
  secrets: { recoveryPassword: randomBytes(24).toString('base64url') },
  output: resolve(root, '.artifacts/e2e/results'),
  cache: { dir: resolve(root, '.artifacts/e2e/cache') },
} satisfies E2EConfig;
