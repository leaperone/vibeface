/** Canonicalize Core `Access` URLs to the Inspector `local_listen` origin (loopback + port). */

function loopbackHost(hostname: string): '127.0.0.1' | '[::1]' | null {
  const h = hostname.toLowerCase()
  if (h === '127.0.0.1' || h === 'localhost') return '127.0.0.1'
  if (h === '[::1]' || h === '::1') return '[::1]'
  return null
}

export type LocalListen = { host: string; port: number }

export function parseLocalListen(text: string): LocalListen | null {
  const trimmed = text.trim()
  const lastColon = trimmed.lastIndexOf(':')
  if (lastColon <= 0) return null
  const hostPart = trimmed.slice(0, lastColon)
  const portPart = trimmed.slice(lastColon + 1)
  const port = Number(portPart)
  if (!Number.isInteger(port) || port <= 0 || port > 65535) return null
  const host = hostPart.replace(/^\[/, '').replace(/\]$/, '')
  return { host, port }
}

/** Rewrite loopback access URLs so the port matches `local_listen` (4791), preserving path/search/hash. */
export function normalizeAccessUrl(raw: string, localListen: LocalListen): string {
  let url: URL
  try {
    url = new URL(raw)
  } catch {
    return raw
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return raw
  const lb = loopbackHost(url.hostname)
  if (!lb) return raw
  const wantHost = lb === '127.0.0.1' ? '127.0.0.1' : '[::1]'
  const match =
    url.hostname.toLowerCase() === wantHost.toLowerCase() ||
    (lb === '127.0.0.1' && url.hostname.toLowerCase() === 'localhost')
  if (!match) return raw
  url.hostname = wantHost
  url.port = String(localListen.port)
  return url.toString()
}
