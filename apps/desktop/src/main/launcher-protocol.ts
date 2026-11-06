import { readFile } from 'node:fs/promises'
import { extname, join, relative, resolve, sep } from 'node:path'
import { protocol } from 'electron'
import { LAUNCHER_HOST, LAUNCHER_SCHEME } from '../shared/contract.js'

/**
 * The launcher is served from its own fixed scheme rather than a local HTTP server, so it
 * has a stable private origin no other process can reach and no port to guess. Only files
 * the build wrote into `dist/launcher` are readable through it.
 */
const TYPES = new Map<string, string>([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.svg', 'image/svg+xml'],
  ['.png', 'image/png'],
  ['.woff2', 'font/woff2'],
])

const POLICY = [
  "default-src 'none'",
  `script-src ${LAUNCHER_SCHEME}://${LAUNCHER_HOST}/`,
  `style-src ${LAUNCHER_SCHEME}://${LAUNCHER_HOST}/`,
  `img-src ${LAUNCHER_SCHEME}://${LAUNCHER_HOST}/ data:`,
  `font-src ${LAUNCHER_SCHEME}://${LAUNCHER_HOST}/`,
  "connect-src 'none'",
  "form-action 'none'",
  "base-uri 'none'",
  "frame-ancestors 'none'",
].join('; ')

export function registerLauncherScheme(): void {
  protocol.registerSchemesAsPrivileged([
    {
      scheme: LAUNCHER_SCHEME,
      privileges: { standard: true, secure: true, supportFetchAPI: false, corsEnabled: false },
    },
  ])
}

/** `rootDir` is the built `dist/launcher` directory; nothing outside it is served. */
export function serveLauncher(rootDir: string): void {
  const root = resolve(rootDir)
  protocol.handle(LAUNCHER_SCHEME, async (request) => {
    const url = new URL(request.url)
    if (request.method !== 'GET' || url.host !== LAUNCHER_HOST) {
      return new Response(null, { status: 404 })
    }
    const name = decodeURIComponent(url.pathname).replace(/^\/+/, '') || 'index.html'
    const target = resolve(join(root, name))
    const inside = relative(root, target)
    if (inside.startsWith('..') || inside.startsWith(sep) || inside === '') {
      return new Response(null, { status: 403 })
    }
    try {
      const body = await readFile(target)
      return new Response(body, {
        status: 200,
        headers: {
          'content-type': TYPES.get(extname(target).toLowerCase()) ?? 'application/octet-stream',
          'content-security-policy': POLICY,
          'cache-control': 'no-store',
          'x-content-type-options': 'nosniff',
        },
      })
    } catch {
      return new Response(null, { status: 404 })
    }
  })
}
