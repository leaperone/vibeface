import { joinTicket, validEndpoint, validTarget } from '../lib/transport.ts'

export function openedJoinLink(value: string | null): string | null {
  if (!value) return null
  try {
    const url = new URL(value.trim())
    if (url.protocol !== 'vibeface:' || url.host !== 'join' || url.pathname || url.hash || url.username || url.password || url.searchParams.has('recover') || url.searchParams.getAll('link').length !== 1) return null
    if ([...url.searchParams.keys()].some(key => key !== 'link')) return null
    const link = url.searchParams.get('link')
    return link && !link.trim().startsWith('vibeface:') && joinTicket(link) ? link : null
  } catch { return null }
}

/** The ticket fixes an entry device, not the network it currently belongs to. */
export function joinDestination(link: string, decode: (ticket: string) => string): { entryId: string; address: string | null } | null {
  if (link.trim().startsWith('vibeface:')) {
    const inner = openedJoinLink(link)
    return inner ? joinDestination(inner, decode) : null
  }
  const found = joinTicket(link)
  if (!found) return null
  try {
    const ticket: unknown = JSON.parse(decode(found.ticket))
    if (!ticket || typeof ticket !== 'object' || !('entry_id' in ticket) || typeof ticket.entry_id !== 'string') return null
    return { entryId: validTarget(ticket.entry_id), address: found.origin ? validEndpoint(found.origin) : null }
  } catch { return null }
}

/**
 * Where a join link leads from a device already in a network: `current` when its entry device is one this device
 * knows in that network, `other` when not, null when it is no join link. The ticket names only the entry device.
 */
export function linkTarget(link: string, decode: (ticket: string) => string, known: readonly string[]): 'current' | 'other' | null {
  const target = joinDestination(link, decode)
  return target ? known.includes(target.entryId) ? 'current' : 'other' : null
}
