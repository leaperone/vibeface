import type { ConnectionEntry, MembershipView } from './node.ts'
import { lanAddrs } from './transport.ts'

/**
 * A member Node from its latest signed address record. `seq` is the highest record accepted, kept
 * even when that record withdrew the web address (`gateway_url` null), so an older record never comes back.
 * `public` means reachable from other networks; `direct` is where the Node listens, for the app on its LAN.
 */
export type Entrance = { device_id: string; seq: number; gateway_url: string | null; public: boolean; direct: string[]; last_ok_ms: number }
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

/** The `ip:port` in a signed iroh endpoint, `{"id", "addrs": [{"Ip": …}, {"Relay": …}]}`. */
function directOf(endpoint: unknown): string[] {
  const addrs = (endpoint as { addrs?: unknown } | null)?.addrs
  return lanAddrs(Array.isArray(addrs) ? addrs.map(addr => (addr as { Ip?: unknown } | null)?.Ip) : [])
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
      const direct = directOf(record.endpoint)
      if (gateway === null) { found.set(deviceId, { device_id: deviceId, seq: seq as number, gateway_url: null, public: false, direct, last_ok_ms: 0 }); continue }
      const url = new URL(gateway)
      if (!['http:', 'https:'].includes(url.protocol) || url.pathname !== '/' || url.search || url.hash || url.username) continue
      found.set(deviceId, { device_id: deviceId, seq: seq as number, gateway_url: url.origin, public: isPublicUrl(url.origin), direct, last_ok_ms: 0 })
    } catch { /* an unreadable record is someone else's problem, not a reason to drop the rest */ }
  }
  return [...found.values()]
}

/** Entrances as this device stored them; any that are not are dropped, and the next connection fetches them again. */
export function storedEntrances(value: unknown): Entrance[] {
  if (!Array.isArray(value)) return []
  return value.filter((item): item is Entrance => {
    const v = item as Partial<Entrance> | null
    return typeof v?.device_id === 'string' && devicePattern.test(v.device_id) && Number.isSafeInteger(v.seq) && (v.gateway_url === null || typeof v.gateway_url === 'string')
      && typeof v.public === 'boolean' && Array.isArray(v.direct) && lanAddrs(v.direct).length === v.direct.length && typeof v.last_ok_ms === 'number'
  })
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
  if (entry.kind === 'lan') return list
  return list.map(item => item.device_id === entry.device_id && item.gateway_url === entry.address ? { ...item, last_ok_ms: at } : item)
}

/**
 * Where to go next, in order: the entry that just failed first, then every other web entrance from most to least
 * recently reached. With `seen`, the member Nodes this app sees on its LAN with where, each of those is tried directly
 * before any web address, the one that failed first, also at the addresses it last signed; a Node without a web
 * address, not seen, is tried directly last.
 */
export function candidates(list: Entrance[], current: ConnectionEntry, seen?: Map<string, string[]>): ConnectionEntry[] {
  const web: ConnectionEntry[] = openEntrances(list)
    .filter(item => current.kind === 'lan' || item.device_id !== current.device_id || item.gateway_url !== current.address)
    .sort((a, b) => b.last_ok_ms - a.last_ok_ms)
    .map(item => ({ kind: 'gateway', address: item.gateway_url, device_id: item.device_id }))
  if (current.kind !== 'lan') web.unshift(current)
  if (!seen) return web
  const direct = new Map<string, string[]>()
  const add = (device: string, addrs: string[]) => { const all = lanAddrs([...direct.get(device) ?? [], ...addrs]); if (all.length) direct.set(device, all) }
  add(current.device_id, [...seen.get(current.device_id) ?? [], ...current.kind === 'lan' ? current.addrs : [], ...list.find(item => item.device_id === current.device_id)?.direct ?? []])
  for (const [device, addrs] of seen) add(device, addrs)
  const lan = (device_id: string, addrs: string[]): ConnectionEntry => ({ kind: 'lan', addrs, device_id })
  const unseen = list.filter(item => item.gateway_url === null && !direct.has(item.device_id) && item.direct.length).map(item => lan(item.device_id, item.direct))
  return [...[...direct].map(([device, addrs]) => lan(device, addrs)), ...web, ...unseen]
}
