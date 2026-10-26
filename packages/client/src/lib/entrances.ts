import type { ConnectionEntry, MembershipView } from './node.ts'

/**
 * A member Gateway from its latest signed address record. `seq` is the highest record accepted, kept
 * even when that record withdrew the web address (`gateway_url` null), so an older record never comes back.
 * `public` means reachable from other networks.
 */
export type Entrance = { device_id: string; seq: number; gateway_url: string | null; public: boolean; last_ok_ms: number }
export type OpenEntrance = Entrance & { gateway_url: string }
type SignedAddress = { payload: string; signature: string }
type Check = (deviceId: string, payload: string, signature: string) => boolean

const devicePattern = /^[a-f0-9]{64}$/
const privateHost = /^(localhost|127\.|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.|169\.254\.|\[::1\]|\[f[cd]|\[fe80:)|\.local$|\.lan$|\.internal$|\.home\.arpa$/i

/** HTTPS on a name other networks can resolve; loopback, LAN and plain HTTP stay local. */
export function isPublicUrl(value: string): boolean {
  try {
    const url = new URL(value)
    return url.protocol === 'https:' && !privateHost.test(url.hostname) && url.hostname.includes('.')
  } catch { return false }
}

const members = (view: MembershipView) => {
  const revoked = new Set(view.revoked.map(item => item.device_id))
  return new Set(view.members.filter(member => member.kind === 'node' && !revoked.has(member.device_id)).map(member => member.device_id))
}

/** Records signed by a current member Node for this network; anything else is dropped. */
export function verifiedEntrances(records: unknown, view: MembershipView, check: Check): Entrance[] {
  if (!Array.isArray(records)) return []
  const nodes = members(view), found = new Map<string, Entrance>()
  for (const item of records.slice(0, 256)) {
    try {
      const { payload, signature } = item as SignedAddress
      if (typeof payload !== 'string' || typeof signature !== 'string' || payload.length > 8192) continue
      const record = JSON.parse(payload) as Record<string, unknown>
      const deviceId = record.device_id, gateway = record.gateway_url, seq = record.seq
      if (record.version !== 1 || record.network_id !== view.network_id || typeof deviceId !== 'string' || !devicePattern.test(deviceId) || !Number.isSafeInteger(seq) || (seq as number) < 0 || (gateway !== null && typeof gateway !== 'string')) continue
      if (!nodes.has(deviceId) || !check(deviceId, payload, signature)) continue
      if ((found.get(deviceId)?.seq ?? -1) >= (seq as number)) continue
      if (gateway === null) { found.set(deviceId, { device_id: deviceId, seq: seq as number, gateway_url: null, public: false, last_ok_ms: 0 }); continue }
      const url = new URL(gateway)
      if (!['http:', 'https:'].includes(url.protocol) || url.pathname !== '/' || url.search || url.hash || url.username) continue
      found.set(deviceId, { device_id: deviceId, seq: seq as number, gateway_url: url.origin, public: isPublicUrl(url.origin), last_ok_ms: 0 })
    } catch { /* an unreadable record is someone else's problem, not a reason to drop the rest */ }
  }
  return [...found.values()]
}

/** A newer record replaces an address, an older one is ignored; success times carry over and revoked Nodes leave. */
export function mergeEntrances(saved: Entrance[], fresh: Entrance[], view: MembershipView): Entrance[] {
  const nodes = members(view)
  const merged = new Map(saved.filter(item => nodes.has(item.device_id)).map(item => [item.device_id, item]))
  for (const item of fresh) {
    const known = merged.get(item.device_id)
    if (known && known.seq >= item.seq) continue
    merged.set(item.device_id, { ...item, last_ok_ms: known?.last_ok_ms ?? 0 })
  }
  return [...merged.values()]
}

/** Entrances whose latest record still names a web address. */
export const openEntrances = (list: Entrance[]): OpenEntrance[] => list.filter((item): item is OpenEntrance => item.gateway_url !== null)

export function markReached(list: Entrance[], entry: ConnectionEntry, at = Date.now()): Entrance[] {
  return list.map(item => item.device_id === entry.device_id && item.gateway_url === entry.address ? { ...item, last_ok_ms: at } : item)
}

/** The entry that just failed first, then every other entrance from most to least recently reached. */
export function candidates(list: Entrance[], current: ConnectionEntry): ConnectionEntry[] {
  const others = openEntrances(list)
    .filter(item => item.device_id !== current.device_id || item.gateway_url !== current.address)
    .sort((a, b) => b.last_ok_ms - a.last_ok_ms)
    .map(item => ({ address: item.gateway_url, device_id: item.device_id }))
  return [current, ...others]
}
