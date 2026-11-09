/**
 * The `vibeface-core --control` request set and validating readers for its replies.
 * Mirrors `vibeface_control::ControlRequest` and `vibeface_core::control`'s JSON results;
 * every reply is parsed field by field so a malformed reply fails here, not deep in the UI.
 */

export type ControlRequest =
  | { command: 'discover_local_nodes'; app_profile_dir: string; remembered_data_dirs: string[] }
  | { command: 'init'; options: InitOptions }
  | { command: 'inspect'; data_dir: string }
  | { command: 'access'; data_dir: string; json: boolean }
  | { command: 'local_setup'; data_dir: string }
  | { command: 'set_local_access'; data_dir: string; enabled: boolean }
  | { command: 'join'; data_dir: string; link: string; name: string }

/** `vibeface_core::control::InitOptions` with only the fields the desktop sets. */
export type InitOptions = { data_dir: string }

export type ControlResponse =
  | { ok: true; result: unknown }
  | { ok: false; error: string }

export type CoreStatus = 'running' | 'stopped' | 'unavailable'

export type Inspect = {
  status: CoreStatus
  deviceId: string
  dataDir: string
  /** `host:port` of the fixed loopback listener that serves the private browser origin. */
  localListen: string
  listen: string
  webEnabled: boolean
  coreVersion: string
  /** Core's own description of why a probe failed; never contains credentials. */
  error: string | null
}

/** A local node: the URL carries a single-use `#local=` or `#bootstrap=` fragment. */
export type Access = { kind: 'local'; url: string; deviceId: string }

function record(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new TypeError(`${what} is not a JSON object`)
  }
  return Object.fromEntries(Object.entries(value))
}

function text(source: Record<string, unknown>, key: string, what: string): string {
  const value = source[key]
  if (typeof value !== 'string' || value.length === 0) {
    throw new TypeError(`${what} is missing ${key}`)
  }
  return value
}

function flag(source: Record<string, unknown>, key: string, what: string): boolean {
  const value = source[key]
  if (typeof value !== 'boolean') throw new TypeError(`${what} is missing ${key}`)
  return value
}

function optionalText(source: Record<string, unknown>, key: string): string | null {
  const value = source[key]
  return typeof value === 'string' && value.length > 0 ? value : null
}

function coreStatus(value: unknown): CoreStatus {
  if (value === 'running' || value === 'stopped' || value === 'unavailable') return value
  throw new TypeError('core inspect returned an unknown status')
}

export function parseControlResponse(value: unknown): ControlResponse {
  const source = record(value, 'core control response')
  const ok = source['ok']
  if (ok === true) return { ok: true, result: source['result'] ?? null }
  if (ok === false) return { ok: false, error: optionalText(source, 'error') ?? 'core control request failed' }
  throw new TypeError('core control response is missing ok')
}

export function parseInspect(value: unknown): Inspect {
  const source = record(value, 'core inspect result')
  return {
    status: coreStatus(source['status']),
    deviceId: text(source, 'device_id', 'core inspect result'),
    dataDir: text(source, 'data_dir', 'core inspect result'),
    localListen: text(source, 'local_listen', 'core inspect result'),
    listen: text(source, 'listen', 'core inspect result'),
    webEnabled: flag(source, 'web_enabled', 'core inspect result'),
    coreVersion: text(source, 'core_version', 'core inspect result'),
    error: optionalText(source, 'error'),
  }
}

export function parseAccess(value: unknown): Access {
  const source = record(value, 'core access result')
  const url = text(source, 'url', 'core access result')
  const deviceId = text(source, 'device_id', 'core access result')
  return { kind: 'local', url, deviceId }
}
