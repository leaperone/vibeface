import { createReadStream, existsSync, statSync } from 'node:fs'
import { extname, join, normalize, resolve } from 'node:path'
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http'

function mime(path: string): string {
  switch (extname(path).toLowerCase()) {
    case '.html':
      return 'text/html; charset=utf-8'
    case '.js':
      return 'application/javascript; charset=utf-8'
    case '.css':
      return 'text/css; charset=utf-8'
    case '.json':
      return 'application/json; charset=utf-8'
    case '.svg':
      return 'image/svg+xml'
    case '.webp':
      return 'image/webp'
    case '.png':
      return 'image/png'
    case '.ico':
      return 'image/x-icon'
    case '.woff2':
      return 'font/woff2'
    default:
      return 'application/octet-stream'
  }
}

/** Local static server for packaged `web/` (Core authoritative UI). No caching to match dev. */
export function startWebStatic(rootDir: string): Promise<{ port: number; close(): Promise<void> }> {
  if (!existsSync(rootDir)) {
    return Promise.reject(new Error(`web root missing: ${rootDir}`))
  }
  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    try {
      const url = new URL(req.url ?? '/', 'http://127.0.0.1')
      let pathname = decodeURIComponent(url.pathname)
      if (pathname.endsWith('/')) pathname += 'index.html'
      const abs = normalize(resolve(join(rootDir, pathname.slice(1))))
      const root = resolve(rootDir)
      if (!abs.startsWith(root + '/') && abs !== root) {
        res.writeHead(403).end()
        return
      }
      if (!existsSync(abs) || !statSync(abs).isFile()) {
        res.writeHead(404).end()
        return
      }
      res.setHeader('Content-Type', mime(abs))
      res.setHeader('Cache-Control', 'no-store')
      createReadStream(abs).pipe(res)
    } catch {
      res.writeHead(500).end()
    }
  })
  return new Promise((resolvePromise, reject) => {
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address()
      const port = typeof addr === 'object' && addr && 'port' in addr ? addr.port : 0
      resolvePromise({
        port,
        close: () =>
          new Promise((r, j) => {
            server.close((err) => (err ? j(err) : r()))
          }),
      })
    })
    server.on('error', reject)
  })
}
