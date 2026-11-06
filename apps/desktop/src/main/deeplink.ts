/** Strict parsing for invite / join deeplinks — no arbitrary protocols or file URIs. */

export type ParsedDeepLink =
  | { kind: 'join'; link: string }
  | { kind: 'none' }
  | { kind: 'invalid'; reason: string }

const JOIN_PATH = '/join'

function hasVibefaceInviteHash(hash: string): boolean {
  return hash.startsWith('#vibeface1')
}

/** `vibeface://join?link=...` — link must be non-empty after trim. */
function parseVibeface(url: URL): ParsedDeepLink {
  if (url.protocol !== 'vibeface:') return { kind: 'invalid', reason: 'unsupported protocol' }
  const host = url.hostname.toLowerCase()
  if (host !== 'join') return { kind: 'invalid', reason: 'vibeface host must be join' }
  const link = url.searchParams.get('link')?.trim() ?? ''
  if (!link) return { kind: 'invalid', reason: 'join link missing' }
  return { kind: 'join', link }
}

/** `http(s)://host/join#vibeface1...` */
function parseWebJoin(url: URL): ParsedDeepLink {
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return { kind: 'none' }
  if (!url.pathname.replace(/\/$/, '').endsWith(JOIN_PATH)) return { kind: 'none' }
  if (!hasVibefaceInviteHash(url.hash)) return { kind: 'none' }
  return { kind: 'join', link: url.toString() }
}

export function parseDeepLink(raw: string): ParsedDeepLink {
  const trimmed = raw.trim()
  if (!trimmed) return { kind: 'none' }
  if (trimmed.startsWith('file:')) return { kind: 'invalid', reason: 'file URLs are not allowed' }
  let url: URL
  try {
    url = new URL(trimmed)
  } catch {
    return { kind: 'invalid', reason: 'invalid URL' }
  }
  if (url.protocol === 'vibeface:') return parseVibeface(url)
  const web = parseWebJoin(url)
  if (web.kind !== 'none') return web
  return { kind: 'none' }
}
