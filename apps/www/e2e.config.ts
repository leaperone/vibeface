import type { E2EConfig } from 'e2e'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const portalRoot = resolve(fileURLToPath(new URL('.', import.meta.url)))
process.env.PLAYWRIGHT_BROWSERS_PATH ??= resolve(portalRoot, '.artifacts/review-www/browsers')
const { web } = await import('@e2e-dev/web')

export default {
  tests: ['./tests/**/*.e2e.ts'],
  targets: [{
    name: 'www',
    engine: web(),
    app: {
      url: 'http://127.0.0.1:0',
      command: {
        executable: 'python3',
        args: ['-m', 'http.server', '{port}', '--bind', '127.0.0.1', '--directory', resolve(portalRoot, 'dist/client')],
        log: resolve(portalRoot, '.artifacts/e2e/server.log'),
      },
    },
  }],
  workers: 1,
  retries: 0,
  output: resolve(portalRoot, '.artifacts/e2e/results'),
} satisfies E2EConfig
