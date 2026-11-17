/**
 * Parse an HTTP(S) origin without page/transport policy. A default port and a single
 * root slash are accepted; the result is for dialing, never a replacement for signed
 * text. Reject URL repairs (whitespace, backslashes, dot paths) as well as credentials,
 * paths, and even empty query/fragment delimiters.
 */
export function parseHttpOrigin(value: unknown): URL | null {
  if (typeof value !== 'string' || /[\u0000-\u0020\u007f]/.test(value) || !/^https?:\/\/[^/?#\\\s]+\/?$/i.test(value)) return null
  try {
    const url = new URL(value)
    const authority = value.slice(value.indexOf('://') + 3).replace(/\/$/, '')
    if (url.username || url.password || authority.includes('@') || authority.endsWith(':') || url.pathname !== '/' || url.search || url.hash) return null
    return url
  } catch { return null }
}

/** An HTTPS origin; whether a private host is allowed belongs to the caller. */
export function parseHttpsOrigin(value: unknown): URL | null {
  const url = parseHttpOrigin(value)
  return url?.protocol === 'https:' ? url : null
}
