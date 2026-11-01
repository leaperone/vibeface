// Keeps the app shell (HTML, scripts, styles, WASM) so the page opens while this Gateway is down.
// Network first: an online Gateway always serves the current build; the cache is only the fallback.
const CACHE = 'vibeface-shell-v1'
const SHELL = ['/', '/generated/vibeface_secure.js', '/generated/vibeface_secure_bg.wasm', '/manifest.webmanifest', '/icon.svg']
const cacheable = url => url.origin === location.origin && !url.pathname.startsWith('/api/') && url.pathname !== '/sw.js'

async function keep(urls) {
  const cache = await caches.open(CACHE)
  await Promise.all(urls.map(async value => {
    const url = new URL(value, location.origin)
    if (!cacheable(url)) return
    const response = await fetch(url).catch(() => null)
    if (response?.ok) await cache.put(url.pathname === '/' ? '/' : url, response)
  }))
}

self.addEventListener('install', event => event.waitUntil((async () => {
  await keep(SHELL)
  const shell = await (await caches.open(CACHE)).match('/')
  const html = shell ? await shell.text() : ''
  await keep([...html.matchAll(/["'](\/assets\/[^"']+)["']/g)].map(match => match[1]))
  await self.skipWaiting()
})()))

self.addEventListener('activate', event => event.waitUntil((async () => {
  for (const key of await caches.keys()) if (key !== CACHE) await caches.delete(key)
  await self.clients.claim()
})()))

// The page reports what it loaded before this worker controlled it.
self.addEventListener('message', event => {
  if (Array.isArray(event.data?.keep)) event.waitUntil(keep(event.data.keep.filter(value => typeof value === 'string').slice(0, 512)))
})

self.addEventListener('fetch', event => {
  const url = new URL(event.request.url)
  if (event.request.method !== 'GET' || !cacheable(url)) return
  const key = event.request.mode === 'navigate' ? '/' : event.request
  event.respondWith((async () => {
    const cache = await caches.open(CACHE)
    try {
      const response = await fetch(event.request)
      if (response.ok) await cache.put(key, response.clone())
      return response
    } catch (error) {
      const cached = await cache.match(key)
      if (cached) return cached
      throw error
    }
  })())
})
