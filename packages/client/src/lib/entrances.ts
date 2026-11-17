import type { ConnectionEntry, MembershipView } from './node.ts'
import { lanAddrs, validEndpoint } from './transport.ts'
import { parseHttpOrigin, parseHttpsOrigin } from './origin.ts'
import publicBootstrap from '../../../../config/public-bootstrap.json' with { type: 'json' }

/** The official page: static, with no Node behind it. */
export const APP_ORIGIN = 'https://app.vibeface.dev'
/**
 * The official public node, the way in before this device has reached any Node: it forwards to any Node by device ID
 * and takes the official page's origin. Once connected, the Node names other public gateways as well.
 */
export const PUBLIC_GATEWAY: string = publicBootstrap.gateway_url

/**
 * Where this device reaches a Node by device ID, as a join link names one, unless the person chose another: the Node
 * that served this page over HTTP(S), which takes its own origin; otherwise the official public node, for the official
 * page and for the apps, whose pages come from their own bundle (`vibeface-app://app` on the desktop).
 */
export function defaultGateway(pageOrigin: string | undefined): string {
  const served = parseHttpOrigin(pageOrigin)
  return served && served.origin !== APP_ORIGIN ? served.origin : PUBLIC_GATEWAY
}

/** A chosen access address as stored, or null when there is none or it no longer validates. */
export function storedGateway(value: unknown): string | null {
  try { return typeof value === 'string' ? validEndpoint(value) : null } catch { return null }
}

/**
 * A member Node from its latest signed address record. `seq` is the highest record accepted, kept
 * even when that record withdrew the web address (`gateway_url` null), so an older record never comes back.
 * `gateway_url` keeps the signed descriptor verbatim; openEntrances derives its dial origin.
 * `public` means reachable from other networks; `direct` is where the Node listens, for the app on its LAN.
 */
export type Entrance = { device_id: string; seq: number; gateway_url: string | null; public: boolean; direct: string[]; last_ok_ms: number }
export type OpenEntrance = Entrance & { gateway_url: string }
type SignedAddress = { payload: string; signature: string }
type Check = (deviceId: string, payload: string, signature: string) => boolean

const devicePattern = /^[a-f0-9]{64}$/
const privateHost = /^(localhost|127\.|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.|169\.254\.|\[::1\]|\[f[cd]|\[fe80:)|\.local$|\.lan$|\.internal$|\.home\.arpa$/i

const publicOrigin = (url: URL): boolean => {
  const host = url.hostname.replace(/\.$/, '')
  return url.protocol === 'https:' && !privateHost.test(host) && host.includes('.')
}

/** HTTPS on a name other networks can resolve; loopback, LAN and plain HTTP stay local. */
export function isPublicUrl(value: string): boolean {
  const url = parseHttpsOrigin(value)
  return url !== null && publicOrigin(url)
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
      const url = parseHttpOrigin(gateway)
      if (!url) continue
      found.set(deviceId, { device_id: deviceId, seq: seq as number, gateway_url: gateway, public: publicOrigin(url), direct, last_ok_ms: 0 })
    } catch { /* an unreadable record is someone else's problem, not a reason to drop the rest */ }
  }
  return [...found.values()]
}

/** Entrances as this device stored them; any that are not are dropped, and the next connection fetches them again. */
export function storedEntrances(value: unknown): Entrance[] {
  if (!Array.isArray(value)) return []
  return value.filter((item): item is Entrance => {
    if (item === null || typeof item !== 'object' || Array.isArray(item)) return false
    const v = item as Partial<Entrance>
    if (typeof v.device_id !== 'string' || !devicePattern.test(v.device_id) || typeof v.seq !== 'number' || !Number.isSafeInteger(v.seq) || v.seq < 0
      || typeof v.last_ok_ms !== 'number' || !Number.isSafeInteger(v.last_ok_ms) || v.last_ok_ms < 0 || typeof v.public !== 'boolean'
      || !Array.isArray(v.direct) || lanAddrs(v.direct).length !== v.direct.length) return false
    if (v.gateway_url === null) return v.public === false
    const url = parseHttpOrigin(v.gateway_url)
    return url !== null && v.public === publicOrigin(url)
  })
}

/** A public gateway outside this network that the Node checked serves its device ID; it forwards to any Node by device ID. */
export type Gateway = { device_id: string; url: string }

/** Gateways as the Node returns them and this device stores them: HTTPS origins only, at most 16, anything else dropped. */
export function gatewaysOf(value: unknown): Gateway[] {
  if (!Array.isArray(value)) return []
  const found = new Map<string, Gateway>()
  for (const item of value.slice(0, 16)) {
    const { device_id, url } = (item ?? {}) as Partial<Gateway>
    if (typeof device_id !== 'string' || !devicePattern.test(device_id) || typeof url !== 'string') continue
    const origin = parseHttpsOrigin(url)
    if (origin) found.set(origin.origin, { device_id, url: origin.origin })
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

/** Dial origins derived from each latest descriptor; the signed/cache descriptor stays untouched. */
export const openEntrances = (list: Entrance[]): OpenEntrance[] => list.flatMap(item => {
  const url = parseHttpOrigin(item.gateway_url)
  return url ? [{ ...item, gateway_url: url.origin }] : []
})

export function markReached(list: Entrance[], entry: ConnectionEntry, at = Date.now()): Entrance[] {
  if (entry.kind === 'lan') return list
  const origin = parseHttpOrigin(entry.address)?.origin
  return list.map(item => item.device_id === entry.device_id && origin !== undefined && parseHttpOrigin(item.gateway_url)?.origin === origin ? { ...item, last_ok_ms: at } : item)
}

/**
 * Where to go next, in order: the entry that just failed first, then every other web entrance from most to least
 * recently reached. With `seen`, the member Nodes this app sees on its LAN right now with where, the current Node is
 * tried directly first and the others before the other web entrances. Only addresses seen now are dialed: a dial names
 * the Node to whoever holds that address, which on another LAN is a stranger. When no web address is left, the current
 * Node is tried where it was last reached or last signed it listens, as a last resort. `gateways` reach the current Node
 * by its device ID after every member's web entrance and before that last resort.
 */
export function candidates(list: Entrance[], current: ConnectionEntry, seen?: Map<string, string[]>, gateways: Gateway[] = []): ConnectionEntry[] {
  const web: ConnectionEntry[] = openEntrances(list)
    .filter(item => {
      if (current.kind === 'lan') return true
      return item.device_id !== current.device_id || item.gateway_url !== current.address
    })
    .sort((a, b) => b.last_ok_ms - a.last_ok_ms)
    .map(item => ({ kind: 'gateway', address: item.gateway_url, device_id: item.device_id }))
  const relayed: ConnectionEntry[] = gateways
    .filter(item => current.kind === 'lan' || item.url !== current.address)
    .map(item => ({ kind: 'gateway', address: item.url, device_id: current.device_id }))
  const own = current.kind === 'lan' ? [] : [current]
  if (!seen) return [...own, ...web, ...relayed]
  const lan = (device_id: string, addrs: string[]): ConnectionEntry => ({ kind: 'lan', addrs, device_id })
  const here = lanAddrs(seen.get(current.device_id))
  const others = [...seen].filter(([device]) => device !== current.device_id).map(([device, addrs]) => lan(device, lanAddrs(addrs))).filter(entry => entry.kind === 'lan' && entry.addrs.length)
  const ways = [...here.length ? [lan(current.device_id, here)] : [], ...own, ...others, ...web]
  if (ways.some(entry => entry.kind !== 'lan')) return [...ways, ...relayed]
  const last = lanAddrs([...current.kind === 'lan' ? current.addrs : [], ...list.find(item => item.device_id === current.device_id)?.direct ?? []])
    .filter(addr => !here.includes(addr))
  return [...ways, ...relayed, ...last.length ? [lan(current.device_id, last)] : []]
}
