/**
 * Only an invite is accepted from the outside world. A deeplink or a second instance can
 * never carry a data directory, a runtime path or an executable: this parser returns only
 * the ticket.
 *
 * Shapes, matching `@vibeface/client`'s `joinTicket`: `vibeface://join/vibeface1…` or
 * `vibeface1…` alone.
 */

export type DeepLink =
  | { kind: 'join'; ticket: string }
  | { kind: 'ignored' }

const TICKET = /^vibeface1[0-9a-z]+$/
const JOIN_LINK = 'vibeface://join/'
const MAX_LINK = 4096

export function parseDeepLink(raw: string): DeepLink {
  const input = raw.trim()
  if (input.length > MAX_LINK) return { kind: 'ignored' }
  const ticket = input.startsWith(JOIN_LINK) ? input.slice(JOIN_LINK.length) : input
  return TICKET.test(ticket) ? { kind: 'join', ticket } : { kind: 'ignored' }
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
