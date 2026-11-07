/**
 * Only an invite is accepted from the outside world. A deeplink or a second instance can
 * never carry a data directory, a runtime path or an executable: this parser returns the
 * ticket and, at most, the entry origin the invite was published at.
 *
 * Shapes, matching `@vibeface/client`'s `joinTicket`: `vibeface1…` alone,
 * `https://<entry>/join#vibeface1…`, or either inside `vibeface://join?link=…`.
 */

export type DeepLink =
  | { kind: 'join'; ticket: string; origin: string | null }
  | { kind: 'ignored' }

const TICKET = /^vibeface1[0-9a-z]+$/
const MAX_LINK = 4096

function joinOrigin(url: URL): string | null {
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return null
  if (url.pathname.replace(/\/+$/, '') !== '/join') return null
  return url.origin
}

function fromUrl(raw: string, allowWrapper: boolean): DeepLink {
  let url: URL
  try {
    url = new URL(raw)
  } catch {
    return { kind: 'ignored' }
  }
  if (url.protocol === 'vibeface:') {
    if (!allowWrapper || url.host !== 'join') return { kind: 'ignored' }
    const inner = url.searchParams.get('link')?.trim() ?? ''
    return inner ? parseDeepLink(inner, false) : { kind: 'ignored' }
  }
  const origin = joinOrigin(url)
  if (origin === null) return { kind: 'ignored' }
  const ticket = url.hash.slice(1)
  return TICKET.test(ticket) ? { kind: 'join', ticket, origin } : { kind: 'ignored' }
}

export function parseDeepLink(raw: string, allowWrapper = true): DeepLink {
  const input = raw.trim()
  if (!input || input.length > MAX_LINK) return { kind: 'ignored' }
  if (TICKET.test(input)) return { kind: 'join', ticket: input, origin: null }
  return fromUrl(input, allowWrapper)
}

/** Pick the one invite out of a process argv or a second-instance argv. */
export function deepLinkFromArgv(argv: readonly string[]): DeepLink {
  for (const argument of argv) {
    if (typeof argument !== 'string') continue
    if (argument.startsWith('-')) continue
    const parsed = parseDeepLink(argument)
    if (parsed.kind === 'join') return parsed
  }
  return { kind: 'ignored' }
}
