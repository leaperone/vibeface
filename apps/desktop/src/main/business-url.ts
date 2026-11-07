/**
 * The business window loads the UI Core itself serves, at Core's own private origin.
 *
 * `access` builds its URL from the public listener (`http://localhost:4790`), but
 * `/api/local/exchange` and the local session cookie only exist on the fixed loopback
 * listener Core reports as `local_listen` (`127.0.0.1:4791` by default), and that router
 * matches the request's literal `Host`. So the reply is canonicalized onto the private
 * origin, keeping the path and the single-use `#local=` / `#bootstrap=` fragment intact.
 */

import type { Access, Inspect } from './control-types.js'

export type BusinessNavigation =
  | { kind: 'business'; url: string }
  | { kind: 'external'; url: string }

export type JoinRequest = { origin: string | null; ticket: string }

export type PrivateOrigin = { host: string; port: number; origin: string }

const HEX64 = /^[0-9a-f]{64}$/

function loopback(host: string): boolean {
  const bare = host.replace(/^\[/, '').replace(/\]$/, '').toLowerCase()
  if (bare === 'localhost') return true
  if (bare === '::1') return true
  if (!/^\d{1,3}(\.\d{1,3}){3}$/.test(bare)) return false
  const octets = bare.split('.').map(Number)
  return octets.every((octet) => octet >= 0 && octet <= 255) && octets[0] === 127
}

/** Read `host:port` exactly as Core serialized its `local_listen` socket address. */
export function parsePrivateOrigin(localListen: string): PrivateOrigin {
  const separator = localListen.lastIndexOf(':')
  if (separator <= 0) throw new TypeError('core reported an unreadable local_listen address')
  const host = localListen.slice(0, separator)
  const port = Number(localListen.slice(separator + 1))
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new TypeError('core reported an unreadable local_listen port')
  }
  if (!loopback(host)) throw new Error('core local_listen is not a loopback address')
  return { host, port, origin: `http://${host}:${port}` }
}

/** The `#local=<token>&node=<device>` fragment is single use: it is validated, never stored or logged. */
function checkLocalFragment(fragment: string, deviceId: string): void {
  const parameters = new URLSearchParams(fragment.replace(/^#/, ''))
  const bootstrap = parameters.get('bootstrap')
  const token = parameters.get('local')
  if (token === null && bootstrap === null) return
  if (bootstrap !== null) {
    if (!/^[A-Za-z0-9_-]+$/.test(bootstrap)) throw new Error('core returned an unreadable bootstrap entry')
    return
  }
  if (token === null || !HEX64.test(token)) throw new Error('core returned an unreadable local entry')
  if (parameters.get('node') !== deviceId) {
    throw new Error('core local entry belongs to another node')
  }
}

/**
 * Resolve the URL the business window may load. A public-entry node is opened as published;
 * a local node is pinned to its own private origin and must match the inspected device.
 */
export function businessUrl(access: Access, inspect: Inspect): string {
  if (access.deviceId !== inspect.deviceId) {
    throw new Error('core access and inspect report different nodes')
  }
  const url = new URL(access.url)
  if (access.kind === 'public_entry') {
    if (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback(url.hostname))) {
      throw new Error('a public entry must publish an HTTPS origin')
    }
    return url.toString()
  }
  if (url.protocol !== 'http:' || !loopback(url.hostname)) {
    throw new Error('core access did not return a loopback URL')
  }
  checkLocalFragment(url.hash, inspect.deviceId)
  const target = new URL(`${parsePrivateOrigin(inspect.localListen).origin}${url.pathname}${url.search}`)
  target.hash = url.hash
  return target.toString()
}

/** Resolve invite ownership from the selected Core's inspected listeners and access URL. */
export function businessNavigation(
  access: Access,
  inspect: Inspect,
  invite?: JoinRequest
): BusinessNavigation {
  const target = businessUrl(access, inspect)
  if (!invite) return { kind: 'business', url: target }

  const targetOrigin = new URL(target).origin
  if (invite.origin !== null && !ownsInviteOrigin(invite.origin, access, inspect, targetOrigin)) {
    return { kind: 'external', url: `${invite.origin}/join#${invite.ticket}` }
  }
  const join = new URL('/join', targetOrigin)
  join.hash = invite.ticket
  return { kind: 'business', url: join.toString() }
}

function ownsInviteOrigin(origin: string, access: Access, inspect: Inspect, targetOrigin: string): boolean {
  const selected = [targetOrigin, new URL(access.url).origin, parsePrivateOrigin(inspect.localListen).origin]
  return selected.some((candidate) => sameListenerOrigin(origin, candidate))
}

function sameListenerOrigin(left: string, right: string): boolean {
  let a: URL
  let b: URL
  try {
    a = new URL(left)
    b = new URL(right)
  } catch {
    return false
  }
  if (a.origin === b.origin) return true
  if (a.protocol !== 'http:' || b.protocol !== 'http:' || effectivePort(a) !== effectivePort(b)) return false
  return (a.hostname === 'localhost' && b.hostname === '127.0.0.1')
    || (b.hostname === 'localhost' && a.hostname === '127.0.0.1')
}

function effectivePort(url: URL): string {
  return url.port || (url.protocol === 'https:' ? '443' : '80')
}
