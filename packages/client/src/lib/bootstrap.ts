import type { SecureApi } from '../secure-api.ts'
import type { ConnectionEntry } from './node.ts'
import { validTarget } from './transport.ts'
import publicDefaults from '../../../../config/public-bootstrap.json' with { type: 'json' }

const encoder = new TextEncoder()

/** Single source of public defaults (root-owned); do not duplicate issuer/host strings in code. */
export type PublicBootstrapDefaults = {
  issuer: string
  udp_host: string
  udp_port: number
  https_base: string
}

export const publicBootstrapDefaults: PublicBootstrapDefaults = publicDefaults

const devicePattern = /^[a-f0-9]{64}$/
const hexSignaturePattern = /^[a-f0-9]+$/

const MAX_SIGNED_FRAME_BYTES = 4096
const MAX_CATALOG_ENTRIES = 4
const FETCH_TIMEOUT_MS = 12_000
/** Ed25519 signature as hex. */
const SIGNATURE_HEX_LENGTH = 128
const MAX_ORIGIN_BYTES = 256
const MAX_CATALOG_LIFETIME_MS = 7 * 24 * 60 * 60 * 1000
const MAX_CACHE_BYTES = 8192

export type PublicBootstrapMode =
  | { kind: 'default' }
  | { kind: 'off' }
  | { kind: 'custom'; issuer: string; https_base: string }

export type CatalogEntry = { gateway_url: string; relay_url: string }

export type VerifiedCatalog = {
  defaults: PublicBootstrapDefaults
  issuer: string
  issued_at_ms: number
  expires_at_ms: number
  entries: CatalogEntry[]
  /** Raw `payload` string bytes as verified (UTF-8). */
  payloadUtf8: Uint8Array
}

type SignedCatalogWire = { payload: string; signature: string }

type BootstrapStorage = {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
  removeItem?(key: string): void
}

/** Namespaced like other client persisted keys (`vibeface.*`). */
const CACHE_KEY = 'vibeface.bootstrap-catalog-cache-v1'
const MODE_KEY = 'vibeface.bootstrap-public-mode'
const CUSTOM_BASE_KEY = 'vibeface.bootstrap-public-custom-base'
const CUSTOM_ISSUER_KEY = 'vibeface.bootstrap-public-custom-issuer'

export function catalogSourceKey(issuer: string, httpsBase: string): string {
  return `${issuer}:${httpsBase}`
}

function modeEffectiveDefaults(mode: PublicBootstrapMode, root: PublicBootstrapDefaults): PublicBootstrapDefaults {
  if (mode.kind === 'custom') return { ...root, issuer: mode.issuer, https_base: mode.https_base }
  return root
}

function catalogUrl(httpsBase: string): string {
  const base = httpsBase.replace(/\/?$/, '')
  return `${base}/catalog`
}

/** True when the catalogue is fetched over HTTP to loopback (fixtures / explicit custom only). */
function allowLoopbackCatalogFetch(mode: PublicBootstrapMode, catalogUrlParsed: URL): boolean {
  if (catalogUrlParsed.protocol !== 'http:') return false
  const h = catalogUrlParsed.hostname
  const local = h === '127.0.0.1' || h === 'localhost' || h === '[::1]'
  if (!local) return false
  return mode.kind === 'custom'
}

function parseIssuerHex(raw: string): string | null {
  const s = raw.trim().toLowerCase()
  if (!devicePattern.test(s)) return null
  return s
}

/**
 * Normalize and validate custom HTTPS base (or loopback HTTP for advanced fixtures).
 * Returns normalized base string without trailing slash (except root path).
 */
export function parseCustomHttpsBaseInput(raw: string): string | null {
  const t = raw.trim()
  if (!t) return null
  if (!(t.startsWith('https://') || t.startsWith('http://127.0.0.1') || t.startsWith('http://localhost') || t.startsWith('http://[::1]'))) return null
  try {
    const u = new URL(t)
    if (u.username || u.password || u.hash) return null
    if (u.search) return null
    const path = u.pathname === '' ? '/' : u.pathname
    const base = `${u.origin}${path === '/' ? '' : path.replace(/\/$/, '')}`
    if (u.protocol === 'https:') return base
    if (u.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(u.hostname)) return base
  } catch {
    return null
  }
  return null
}

function parseMode(storage: BootstrapStorage | null): PublicBootstrapMode {
  if (!storage) return { kind: 'default' }
  const mode = storage.getItem(MODE_KEY)
  if (mode === 'off') return { kind: 'off' }
  if (mode === 'custom') {
    const rawBase = storage.getItem(CUSTOM_BASE_KEY)
    const rawIssuer = storage.getItem(CUSTOM_ISSUER_KEY)
    if (typeof rawBase !== 'string' || typeof rawIssuer !== 'string') return { kind: 'off' }
    const issuer = parseIssuerHex(rawIssuer)
    const https_base = parseCustomHttpsBaseInput(rawBase)
    if (!issuer || !https_base) return { kind: 'off' }
    return { kind: 'custom', issuer, https_base }
  }
  return mode === null || mode === '' || mode === 'default' ? { kind: 'default' } : { kind: 'off' }
}

/** Read-only; advanced UI may set keys via `setPublicBootstrapMode`. */
export function getPublicBootstrapMode(storage: BootstrapStorage | null): PublicBootstrapMode {
  try { return parseMode(storage) } catch { return { kind: 'off' } }
}

export function setPublicBootstrapMode(storage: BootstrapStorage, mode: PublicBootstrapMode): void {
  const drop = (key: string) => {
    if (storage.removeItem) storage.removeItem(key)
    else storage.setItem(key, '')
  }
  if (mode.kind === 'default') {
    drop(MODE_KEY)
    drop(CUSTOM_BASE_KEY)
    drop(CUSTOM_ISSUER_KEY)
    return
  }
  if (mode.kind === 'off') {
    storage.setItem(MODE_KEY, 'off')
    drop(CUSTOM_BASE_KEY)
    drop(CUSTOM_ISSUER_KEY)
    return
  }
  storage.setItem(MODE_KEY, 'custom')
  storage.setItem(CUSTOM_BASE_KEY, mode.https_base)
  storage.setItem(CUSTOM_ISSUER_KEY, mode.issuer)
}

/** Persisted cache: signed wire envelope only; verified material is recomputed on load. */
type CatalogCacheV2 = {
  v: 2
  source_key: string
  issuer: string
  https_base: string
  fetched_at_ms: number
  payload: string
  signature: string
}

function parseWireFields(row: unknown): { payload: string; signature: string } | null {
  if (!row || typeof row !== 'object') return null
  const o = row as Record<string, unknown>
  const payload = o.payload
  const signature = o.signature
  if (typeof payload !== 'string' || typeof signature !== 'string') return null
  return { payload, signature }
}

/**
 * Load persisted catalogue only after re-verifying signature and payload shape through `secure`.
 * Corrupt or stale data returns null.
 */
export function readCachedCatalog(
  storage: BootstrapStorage | null,
  mode: PublicBootstrapMode,
  secure: Pick<SecureApi, 'verify'>,
  nowMs = Date.now(),
  rootDefaults: PublicBootstrapDefaults = publicBootstrapDefaults,
): VerifiedCatalog | null {
  if (mode.kind === 'off' || !storage) return null
  const defaults = modeEffectiveDefaults(mode, rootDefaults)
  return readCacheEnvelope(storage, mode, secure, defaults, nowMs)
}

function readCacheEnvelope(
  storage: BootstrapStorage,
  mode: PublicBootstrapMode,
  secure: Pick<SecureApi, 'verify'>,
  defaults: PublicBootstrapDefaults,
  now: number,
): VerifiedCatalog | null {
  try {
    const raw = storage.getItem(CACHE_KEY)
    if (!raw || encoder.encode(raw).byteLength > MAX_CACHE_BYTES) return null
    const parsed: unknown = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object') return null
    const row = parsed as Record<string, unknown>
    if (row.v !== 2) return null
    const expectedKey = catalogSourceKey(defaults.issuer, defaults.https_base)
    if (
      row.source_key !== expectedKey ||
      row.issuer !== defaults.issuer ||
      row.https_base !== defaults.https_base ||
      typeof row.fetched_at_ms !== 'number' || !Number.isSafeInteger(row.fetched_at_ms) ||
      typeof row.payload !== 'string' ||
      typeof row.signature !== 'string'
    ) {
      return null
    }
    let catalogUrlParsed: URL
    try {
      catalogUrlParsed = new URL(catalogUrl(defaults.https_base))
    } catch {
      return null
    }
    const allowLoopbackEntries = allowLoopbackCatalogFetch(mode, catalogUrlParsed)
    try {
      return verifySignedCatalog({ payload: row.payload, signature: row.signature }, secure, defaults, now, {
        allowLoopbackInEntries: allowLoopbackEntries,
      })
    } catch {
      return null
    }
  } catch {
    return null
  }
}

function writeCacheV2(storage: BootstrapStorage | null, row: CatalogCacheV2): void {
  if (!storage) return
  try {
    storage.setItem(CACHE_KEY, JSON.stringify(row))
  } catch {
    /* quota or private mode */
  }
}

function validateHttpsOrigin(url: URL, allowLoopbackHttp: boolean): boolean {
  const local = url.hostname === 'localhost' || url.hostname === '127.0.0.1' || url.hostname === '[::1]'
  if (url.username || url.password || url.hash || url.search) return false
  const pathOk = url.pathname === '/' || url.pathname === ''
  if (url.protocol === 'https:' && pathOk) return true
  if (allowLoopbackHttp && url.protocol === 'http:' && local && pathOk) return true
  return false
}

export type ParseVerifiedCatalogOptions = {
  /** When true (explicit fixture/custom fetch over loopback HTTP), catalogue may list loopback HTTP origins. */
  allowLoopbackInEntries: boolean
}

/**
 * Parse and validate catalog JSON after signature verification.
 * `payloadString` must be the exact string that was signed.
 */
export function parseVerifiedCatalogPayload(
  payloadString: string,
  expectedIssuer: string,
  nowMs: number,
  options: ParseVerifiedCatalogOptions,
): { entries: CatalogEntry[]; issued_at_ms: number; expires_at_ms: number; issuer: string } {
  let inner: unknown
  try {
    inner = JSON.parse(payloadString)
  } catch {
    throw new Error('bootstrap catalogue payload is not valid JSON')
  }
  if (!inner || typeof inner !== 'object' || Array.isArray(inner)) throw new Error('bootstrap catalogue shape invalid')
  const row = inner as Record<string, unknown>
  if (Object.keys(row).some(key => !['version', 'issuer', 'issued_at_ms', 'expires_at_ms', 'entries'].includes(key))) {
    throw new Error('bootstrap catalogue contains unknown fields')
  }
  if (row.version !== 1) throw new Error('bootstrap catalogue version unsupported')
  if (typeof row.issuer !== 'string' || !devicePattern.test(row.issuer)) throw new Error('bootstrap catalogue issuer invalid')
  if (row.issuer !== expectedIssuer) throw new Error('bootstrap catalogue issuer does not match configured defaults')
  if (typeof row.issued_at_ms !== 'number' || !Number.isSafeInteger(row.issued_at_ms)) throw new Error('bootstrap catalogue issued time invalid')
  if (typeof row.expires_at_ms !== 'number' || !Number.isSafeInteger(row.expires_at_ms)) throw new Error('bootstrap catalogue expiry invalid')
  if (row.expires_at_ms <= nowMs || row.issued_at_ms > nowMs + 60_000) throw new Error('bootstrap catalogue is not valid at this time')
  if (row.expires_at_ms <= row.issued_at_ms || row.expires_at_ms > row.issued_at_ms + MAX_CATALOG_LIFETIME_MS) {
    throw new Error('bootstrap catalogue issued/expiry bounds invalid')
  }
  if (!Array.isArray(row.entries)) throw new Error('bootstrap catalogue entries missing')
  if (row.entries.length > MAX_CATALOG_ENTRIES) throw new Error('bootstrap catalogue has too many entries')
  const allowLoopback = options.allowLoopbackInEntries
  const entries: CatalogEntry[] = []
  for (const item of row.entries) {
    if (!item || typeof item !== 'object') throw new Error('bootstrap catalogue entry invalid')
    const e = item as Record<string, unknown>
    if (Object.keys(e).some(key => key !== 'gateway_url' && key !== 'relay_url')) throw new Error('bootstrap catalogue entry contains unknown fields')
    if (typeof e.gateway_url !== 'string' || typeof e.relay_url !== 'string') throw new Error('bootstrap catalogue entry URLs invalid')
    if (encoder.encode(e.gateway_url).byteLength > MAX_ORIGIN_BYTES || encoder.encode(e.relay_url).byteLength > MAX_ORIGIN_BYTES) {
      throw new Error('bootstrap catalogue entry URL too long')
    }
    let gateway: URL
    let relay: URL
    try {
      gateway = new URL(e.gateway_url)
      relay = new URL(e.relay_url)
    } catch {
      throw new Error('bootstrap catalogue URL parse failed')
    }
    if (!validateHttpsOrigin(gateway, allowLoopback) || !validateHttpsOrigin(relay, allowLoopback)) {
      throw new Error('bootstrap catalogue origin not allowed')
    }
    if (encoder.encode(gateway.origin).byteLength > MAX_ORIGIN_BYTES || encoder.encode(relay.origin).byteLength > MAX_ORIGIN_BYTES) {
      throw new Error('bootstrap catalogue normalized origin too long')
    }
    entries.push({ gateway_url: gateway.origin, relay_url: relay.origin })
  }
  return { entries, issued_at_ms: row.issued_at_ms, expires_at_ms: row.expires_at_ms, issuer: row.issuer }
}

export function verifySignedCatalog(
  wire: unknown,
  secure: Pick<SecureApi, 'verify'>,
  defaults: PublicBootstrapDefaults,
  nowMs: number,
  parseOpts: ParseVerifiedCatalogOptions,
): VerifiedCatalog {
  if (!wire || typeof wire !== 'object' || Array.isArray(wire)) throw new Error('bootstrap catalogue response invalid')
  if (Object.keys(wire).some(key => key !== 'payload' && key !== 'signature')) throw new Error('bootstrap catalogue response contains unknown fields')
  const { payload, signature } = wire as SignedCatalogWire
  if (typeof payload !== 'string' || typeof signature !== 'string') throw new Error('bootstrap catalogue wire fields invalid')
  if (encoder.encode(JSON.stringify({ payload, signature })).byteLength > MAX_SIGNED_FRAME_BYTES) throw new Error('bootstrap catalogue response too large')
  if (!hexSignaturePattern.test(signature) || signature.length !== SIGNATURE_HEX_LENGTH) {
    throw new Error('bootstrap catalogue signature format invalid')
  }
  const payloadUtf8 = encoder.encode(payload)
  secure.verify(defaults.issuer, 'bootstrap-catalog', payloadUtf8, signature)
  const parsed = parseVerifiedCatalogPayload(payload, defaults.issuer, nowMs, parseOpts)
  return {
    defaults,
    issuer: parsed.issuer,
    issued_at_ms: parsed.issued_at_ms,
    expires_at_ms: parsed.expires_at_ms,
    entries: parsed.entries,
    payloadUtf8,
  }
}

async function readResponseBody(res: Response, maxBytes: number): Promise<Uint8Array> {
  const reader = res.body?.getReader()
  if (!reader) {
    const buffer = new Uint8Array(await res.arrayBuffer())
    if (buffer.length > maxBytes) throw new Error('bootstrap catalogue response too large')
    return buffer
  }
  const chunks: Uint8Array[] = []
  let total = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    if (!value) continue
    total += value.byteLength
    if (total > maxBytes) {
      reader.cancel().catch(() => {})
      throw new Error('bootstrap catalogue response too large')
    }
    chunks.push(value)
  }
  const out = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    out.set(chunk, offset)
    offset += chunk.byteLength
  }
  return out
}

export type FetchCatalogOptions = {
  secure: Pick<SecureApi, 'verify'>
  signal?: AbortSignal
  storage?: BootstrapStorage | null
  /** Root JSON defaults; defaults to imported `publicBootstrapDefaults`. */
  defaults?: PublicBootstrapDefaults
  /** Defaults + HTTPS base from config unless mode is custom. */
  mode?: PublicBootstrapMode
  nowMs?: number
}

/**
 * GET signed catalogue from configured HTTPS base; cache positive results until expiry.
 * Rejects redirects, credentials, and oversized bodies.
 */
export async function fetchVerifiedCatalog(options: FetchCatalogOptions): Promise<VerifiedCatalog> {
  const mode = options.mode ?? { kind: 'default' }
  if (mode.kind === 'off') throw new Error('public bootstrap is disabled')
  if (options.signal?.aborted) throw new Error('bootstrap catalogue request was cancelled')
  const root = options.defaults ?? publicBootstrapDefaults
  const defaults = modeEffectiveDefaults(mode, root)
  const now = options.nowMs ?? Date.now()
  const cached = readCachedCatalog(options.storage ?? null, mode, options.secure, now, root)
  if (cached) return cached

  const url = catalogUrl(defaults.https_base)
  let catalogUrlParsed: URL
  try {
    catalogUrlParsed = new URL(url)
  } catch {
    throw new Error('bootstrap catalogue URL invalid')
  }
  if (catalogUrlParsed.protocol !== 'https:' && !allowLoopbackCatalogFetch(mode, catalogUrlParsed)) {
    throw new Error('bootstrap catalogue fetch requires HTTPS')
  }

  const controller = new AbortController()
  const timed = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS)
  const outer = options.signal
  const combined = new AbortController()
  const abortCombined = () => combined.abort()
  outer?.addEventListener('abort', abortCombined, { once: true })
  controller.signal.addEventListener('abort', abortCombined, { once: true })
  const signal = combined.signal
  const allowLoopbackEntries = allowLoopbackCatalogFetch(mode, catalogUrlParsed)
  try {
    const request = {
      method: 'GET',
      credentials: 'omit',
      redirect: 'manual',
      signal,
    } satisfies RequestInit
    const res = await fetch(url, request)
    if (res.type === 'opaqueredirect' || (res.status >= 300 && res.status < 400)) throw new Error('bootstrap catalogue redirect rejected')
    if (!res.ok) throw new Error(`bootstrap catalogue HTTP ${res.status}`)
    const bytes = await readResponseBody(res, MAX_SIGNED_FRAME_BYTES)
    const text = new TextDecoder('utf-8', { fatal: true }).decode(bytes)
    const wire: unknown = JSON.parse(text)
    const fields = parseWireFields(wire)
    if (!fields) throw new Error('bootstrap catalogue response invalid')
    const verified = verifySignedCatalog(wire, options.secure, defaults, now, { allowLoopbackInEntries: allowLoopbackEntries })
    writeCacheV2(options.storage ?? null, {
      v: 2,
      source_key: catalogSourceKey(defaults.issuer, defaults.https_base),
      issuer: defaults.issuer,
      https_base: defaults.https_base,
      fetched_at_ms: now,
      payload: fields.payload,
      signature: fields.signature,
    })
    return verified
  } finally {
    clearTimeout(timed)
    controller.abort()
    outer?.removeEventListener('abort', abortCombined)
    controller.signal.removeEventListener('abort', abortCombined)
  }
}

/** Build gateway connection entries for Node IDs the user is allowed to reach (already in membership hints). */
export function catalogConnectionEntries(catalog: VerifiedCatalog | null, allowedDeviceIds: ReadonlySet<string>): ConnectionEntry[] {
  if (!catalog) return []
  const out: ConnectionEntry[] = []
  for (const entry of catalog.entries) {
    for (const deviceId of allowedDeviceIds) {
      try {
        validTarget(deviceId)
      } catch {
        continue
      }
      try {
        const address = entry.gateway_url
        const url = new URL(address)
        if (url.username || url.password || url.search || url.hash) continue
        out.push({ kind: 'gateway', address: url.origin, device_id: deviceId })
      } catch {
        /* next */
      }
    }
  }
  return dedupeEntries(out)
}

function dedupeEntries(list: ConnectionEntry[]): ConnectionEntry[] {
  const seen = new Set<string>()
  const out: ConnectionEntry[] = []
  for (const item of list) {
    const key = `${item.device_id}:${item.kind === 'gateway' ? item.address : ''}`
    if (seen.has(key)) continue
    seen.add(key)
    out.push(item)
  }
  return out
}

export type BootstrapPrefetch = {
  refresh(options: { secure: Pick<SecureApi, 'verify'>; storage: BootstrapStorage | null; signal?: AbortSignal }): Promise<void>
  snapshot(): VerifiedCatalog | null
}

/** Non-blocking background prefetch; snapshot is read during reconnect/join. */
export function createBootstrapPrefetch(): BootstrapPrefetch {
  let latest: VerifiedCatalog | null = null
  let inFlight: Promise<void> | null = null
  return {
    snapshot: () => latest,
    async refresh(opts) {
      const mode = parseMode(opts.storage)
      if (mode.kind === 'off') {
        latest = null
        return
      }
      if (inFlight) {
        try {
          await inFlight
        } catch {
          /* ignore */
        }
        return
      }
      inFlight = (async () => {
        try {
          latest = await fetchVerifiedCatalog({ secure: opts.secure, storage: opts.storage, signal: opts.signal, mode })
        } catch {
          latest = readCachedCatalog(opts.storage, mode, opts.secure, Date.now())
        } finally {
          inFlight = null
        }
      })()
      try {
        await inFlight
      } catch {
        inFlight = null
      }
    },
  }
}
