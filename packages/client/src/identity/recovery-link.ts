import { openEntrances, type Entrance } from '../lib/entrances.ts'
import { parseEntry, type ConnectionEntry } from '../lib/node.ts'

const loopback = /^https?:\/\/(localhost|127\.|0\.0\.0\.0|\[::1?\])/i

/**
 * What a phone scans or pastes to know which computer to recover through, in the shape of a join link:
 * `vibeface://join?recover=<device>` with its web `address`, or with each `lan` `ip:port` it listens on.
 * It only says where to go; nothing in it lets anyone in.
 */
export function recoveryLink(entry: ConnectionEntry): string {
  const params = new URLSearchParams({ recover: entry.device_id })
  if (entry.kind === 'lan') for (const addr of entry.addrs) params.append('lan', addr)
  else params.set('address', entry.address)
  return `vibeface://join?${params}`
}

/** The computer a recovery link names, or `null` for anything else, a join link included. */
export function recoveryTarget(value: string): ConnectionEntry | null {
  try {
    const url = new URL(value.trim())
    const device_id = url.searchParams.get('recover')
    if (url.protocol !== 'vibeface:' || url.host !== 'join' || !device_id || url.searchParams.has('link')) return null
    const address = url.searchParams.get('address')
    return parseEntry(address ? { device_id, address } : { device_id, addrs: url.searchParams.getAll('lan') })
  } catch { return null }
}

/**
 * Where another phone can reach this network to recover: a public web entrance first, the connected Node's before
 * others, then any web address that is not this computer's loopback, then where the connected Node listens on the LAN.
 */
export function recoveryEntry(entrances: Entrance[], current: ConnectionEntry | null): ConnectionEntry | null {
  const own = (item: { device_id: string }) => Number(item.device_id === current?.device_id)
  const web = openEntrances(entrances).filter(item => !loopback.test(item.gateway_url))
    .sort((a, b) => Number(b.public) - Number(a.public) || own(b) - own(a) || b.last_ok_ms - a.last_ok_ms)
  if (web[0]?.public) return { kind: 'gateway', address: web[0].gateway_url, device_id: web[0].device_id }
  if (current?.kind === 'gateway' && !loopback.test(current.address)) return current
  if (web[0]) return { kind: 'gateway', address: web[0].gateway_url, device_id: web[0].device_id }
  const lan = entrances.filter(item => item.direct.length).sort((a, b) => own(b) - own(a))[0]
  return lan ? { kind: 'lan', addrs: lan.direct, device_id: lan.device_id } : current?.kind === 'lan' ? current : null
}
