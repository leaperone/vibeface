/**
 * The browser's link to its personal network. `catchingUp` means the entry has not yet
 * seen everything this browser has shown; shown content stays while it catches up.
 */
export type Link =
  | { kind: 'booting' }
  | { kind: 'unjoined' }
  | { kind: 'connecting'; attempt: number }
  | { kind: 'online'; catchingUp: boolean }
  | { kind: 'offline'; attempt: number; retryInMs: number; reason: string }
  | { kind: 'revoked' }

export type LinkEvent =
  | { type: 'ready'; joined: boolean }
  | { type: 'connect' }
  | { type: 'connected'; behind: boolean }
  | { type: 'caught-up' }
  | { type: 'lost'; reason: string }
  | { type: 'revoked' }
  | { type: 'left' }

const backoff = (attempt: number) => Math.min(10_000, 500 * 2 ** Math.max(0, attempt - 1))

/** Events that make no sense in the current state leave it unchanged. */
export function nextLink(link: Link, event: LinkEvent): Link {
  if (event.type === 'revoked') return { kind: 'revoked' }
  if (event.type === 'left') return { kind: 'unjoined' }
  switch (link.kind) {
    case 'booting':
    case 'unjoined':
      return event.type === 'ready' ? (event.joined ? { kind: 'connecting', attempt: 0 } : { kind: 'unjoined' }) : link
    case 'connecting':
      if (event.type === 'connected') return { kind: 'online', catchingUp: event.behind }
      if (event.type === 'lost') return { kind: 'offline', attempt: link.attempt + 1, retryInMs: backoff(link.attempt + 1), reason: event.reason }
      return link
    case 'online':
      if (event.type === 'caught-up') return { kind: 'online', catchingUp: false }
      if (event.type === 'lost') return { kind: 'offline', attempt: 1, retryInMs: backoff(1), reason: event.reason }
      return link
    case 'offline':
      return event.type === 'connect' ? { kind: 'connecting', attempt: link.attempt } : link
    case 'revoked':
      return event.type === 'ready' && event.joined ? { kind: 'connecting', attempt: 0 } : link
  }
}
