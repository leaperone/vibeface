import { joinTicket, validTarget } from '../lib/transport.ts'

/**
 * The entry device a join link or ticket names, null when it is no join link or its ticket names no valid device.
 * The ticket fixes an entry device, not the network it currently belongs to, and carries no address.
 */
export function joinDestination(link: string, decode: (ticket: string) => string): string | null {
  const ticket = joinTicket(link)
  if (!ticket) return null
  try {
    const value: unknown = JSON.parse(decode(ticket))
    return value && typeof value === 'object' && 'entry_id' in value ? validTarget(value.entry_id) : null
  } catch { return null }
}

/**
 * Where a join link leads from a device already in a network: `current` when its entry device is one this device
 * knows in that network, `other` when not, null when it is no join link. The ticket names only the entry device.
 */
export function linkTarget(link: string, decode: (ticket: string) => string, known: readonly string[]): 'current' | 'other' | null {
  const entryId = joinDestination(link, decode)
  return entryId ? known.includes(entryId) ? 'current' : 'other' : null
}
