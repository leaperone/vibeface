import { resolve } from 'node:path';
import type { E2EConfig } from 'e2e';

const artifacts = resolve(import.meta.dirname, '.artifacts/native-desktop');

export default {
  tests: ['native-desktop.e2e.ts'],
  targets: [{ name: 'native-desktop', platform: 'native-desktop' }],
  workers: 1,
  retries: 0,
  timeout: 120000,
  cleanupTimeout: 15000,
  trace: 'off',
  video: 'off',
  cache: 'off',
  output: resolve(artifacts, 'runner'),
} satisfies E2EConfig;
