import { joinTicket } from '../lib/transport.ts'

/**
 * Where a join link leads from a device already in a network: `current` when its entry device is one this device
 * knows in that network, `other` when not, null when it is no join link. The ticket names only the entry device.
 */
export function linkTarget(link: string, decode: (ticket: string) => string, known: readonly string[]): 'current' | 'other' | null {
  const found = joinTicket(link)
  if (!found) return null
  let entryId: unknown
  try { entryId = (JSON.parse(decode(found.ticket)) as { entry_id?: unknown }).entry_id } catch { return null }
  if (typeof entryId !== 'string') return null
  return known.includes(entryId) ? 'current' : 'other'
}
