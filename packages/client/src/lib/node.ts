import { lanAddrs, validEndpoint, validTarget } from './transport.ts'
import type { ChangedEvent } from './progress.ts'
import { humanError } from './errors.ts'

/**
 * How this device reaches a Node: through a Gateway's web address, over the cookie-bound local page of the computer
 * it runs on, or straight to it on this LAN (the app only) at the `ip:port` it was last seen at.
 */
export type ConnectionEntry =
  | { kind: 'gateway'; address: string; device_id: string }
  | { kind: 'local'; address: string; device_id: string }
  | { kind: 'lan'; addrs: string[]; device_id: string }
export type PublicIdentity = { deviceId: string }
export type ClientPrincipal = { network_id: string; device_id: string; epoch: number } & ({ source: 'membership'; can_manage: boolean } | { source: 'local_session'; can_manage: false })
export type EntryInfo = { device_id: string; name: string; version: string; initialized: boolean; network_id: string | null; connection_entry?: ConnectionEntry }
export type ReadyConnection = {
  readonly generation: number
  entry: ConnectionEntry
  principal: ClientPrincipal
  info: EntryInfo
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  rpc(method: string, params?: Record<string, unknown>): Promise<unknown>
  subscribe(listener: (event: ChangedEvent) => void): () => void
  isCurrent(): boolean
  /** The entry has not yet seen everything this page showed; keep what is on screen meanwhile. */
  catchingUp?(): boolean
}
export type Member = { device_id: string; kind: 'node' | 'client'; name: string; manage: boolean }
type Revocation = { device_id: string; cutoff_seq: number; cutoff_hash: string | null }
/** A recovery that replaces the directory once `effective_at_ms` passes, unless a manager of this one stops it. */
export type PendingRecovery = { checkpoint: string; effective_at_ms: number; members: Member[] }
export type MembershipView = { network_id: string; epoch: number; recovery_key: string; heads: string[]; members: Member[]; revoked: Revocation[]; attesters: string[]; pending: PendingRecovery | null; stopped: string[] }
type JoinStatus = 'pending' | 'confirming' | 'joined' | 'rejected' | 'withdrawn' | 'expired'
/** One join request as the entry Node stores it; `code` is filled in once both nonces are known. */
export type JoinRequest = {
  join_id: string; status: JoinStatus; network_id: string; invite_id: string; device_id: string; kind: 'node' | 'client'; name: string; commit: string
  admin_id: string | null; admin_nonce: string | null; joiner_nonce: string | null; reason: string | null; expires_at_ms: number; code?: string
}
export type LocalSetup = { kind: 'uninitialized'; entry: ConnectionEntry; capability: string } | { kind: 'initialized'; entry: ConnectionEntry; removed: boolean; local_launch?: { url: string; device_id: string; token: string } }
export type ManagerSettings = { provider: ProviderId; workspace: string; model: string }
export type NodeSettings = { roots: string[]; manager: ManagerSettings | null }
export type ProviderId = 'codex' | 'cursor' | 'claude' | 'pi' | 'opencode'
export const providerNames: Record<ProviderId, string> = { codex: 'Codex', cursor: 'Cursor', claude: 'Claude Code', pi: 'pi', opencode: 'opencode' }
type ProviderRole = 'agent' | 'execution'
export type ProviderIssue = { code: 'bin_missing'; path: string } | { code: 'env_missing'; name: string } | { code: 'launch_failed'; detail: string } | { code: 'config_invalid' }
  | { code: 'app_server_not_found' } | { code: 'app_server_unreachable'; detail: string }
/** The Codex App Server as the device sees it, live or from a fresh handshake. */
export type AppServerLink = { state: 'off' } | { state: 'starting' } | { state: 'connected'; socket: string; user_agent: string }
  | { state: 'not_found'; candidates: string[] } | { state: 'handshake_failed'; socket: string; reason: string } | { state: 'launch_failed'; reason: string }
export type AppServerMode = 'system' | 'spawn'
/** `socket` is required when enabling a system App Server connection. */
export type AppServer = { mode: AppServerMode; socket: string | null; link: AppServerLink }
/**
 * One local provider of the connected Node. `roles` is what it runs now: `agent` while the bot
 * runs on it, `execution` while it executes tasks; `off` means it runs nothing.
 * Variable values never come back, only whether each is set.
 */
export type ProviderStatus = {
  provider: ProviderId; roles: ProviderRole[]; supports: ProviderRole[]; bin: string; app_server: AppServer | null
  /** Whether its program is on that device. */
  installed: boolean
  /** Codex only: the Codex folder and the sessions folder the device uses. */
  home: string | null; sessions: string | null
  env: { name: string; set: boolean; inherited: boolean; required: boolean }[]
  state: 'ready' | 'needs_setup' | 'starting' | 'off'; issues: ProviderIssue[]
}
export type NodeSettingsResponse = {
  settings: NodeSettings
  defaults: Pick<ManagerSettings, 'workspace'>
  readiness: { manager: 'disabled' | 'authentication_unverified' | 'missing_prerequisites'; missing: ProviderId[] }
  providers: ProviderStatus[]
  /** How Settings shows each path under the home folder (`~/…`); edits still use the full path. */
  display_paths: Record<string, string>
  /** Usual project folders on that device not yet among its folders, offered with one click. */
  suggested_roots: string[]
}

export function record(value: unknown, label = '设备响应'): Record<string, unknown> {
  if (!isRecord(value)) throw new Error(`${label}格式无效。`)
  return value
}
function isRecord(value: unknown): value is Record<string, unknown> { return value !== null && typeof value === 'object' && !Array.isArray(value) }
export function text(value: unknown, label = '文本'): string {
  if (typeof value !== 'string' || !value) throw new Error(`${label}缺失或格式无效。`)
  return value
}
export function boolean(value: unknown): boolean { if (typeof value !== 'boolean') throw new Error('设备返回的状态格式无效。'); return value }
function integer(value: unknown): number { if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) throw new Error('设备返回的编号或版本无效。'); return value }
function strings(value: unknown): string[] { if (!Array.isArray(value)) throw new Error('设备返回的列表格式无效。'); return value.map(item => text(item)) }
export function array(value: unknown): unknown[] { if (!Array.isArray(value)) throw new Error('设备返回的列表格式无效。'); return value }
/** An entry as Nodes send it and this device stores it: `{address, device_id}`, plus `local: true` for a local page, or `{device_id, addrs}` on the LAN. */
export function parseEntry(value: unknown): ConnectionEntry {
  const v = record(value, '访问地址')
  const device_id = validTarget(v.device_id)
  if (v.address === undefined) {
    const addrs = lanAddrs(v.addrs)
    if (!addrs.length) throw new Error('访问地址缺失或格式无效。')
    return { kind: 'lan', addrs, device_id }
  }
  const address = validEndpoint(text(v.address))
  if (v.local !== true) return { kind: 'gateway', address, device_id }
  if (address !== (globalThis as { location?: { origin: string } }).location?.origin) throw new Error('本机打开时只能连这台电脑。')
  return { kind: 'local', address, device_id }
}
export const entryJson = (entry: ConnectionEntry) => entry.kind === 'lan' ? { device_id: entry.device_id, addrs: entry.addrs }
  : { address: entry.address, device_id: entry.device_id, ...entry.kind === 'local' ? { local: true } : {} }
/** Where `entry` goes, as people read it: its web address, or on the LAN the `ip:port` it was last reached at. */
export const entryAddress = (entry: ConnectionEntry) => entry.kind === 'lan' ? entry.addrs[0] : entry.address
/** One key per way in; a Node has one LAN entry, whose addresses change. */
export const entryKey = (entry: ConnectionEntry) => entry.kind === 'lan' ? `lan:${entry.device_id}` : `${entry.address}:${entry.device_id}:${entry.kind === 'local'}`
export function parseInfo(value: unknown): EntryInfo {
  const v = record(value)
  const initialized = boolean(v.initialized)
  return {
    device_id: validTarget(v.device_id), name: text(v.name), version: text(v.version), initialized,
    network_id: v.network_id === null ? null : validTarget(v.network_id),
    ...(v.connection_entry === undefined ? {} : { connection_entry: parseEntry(v.connection_entry) }),
  }
}
function parseMember(value: unknown): Member {
  const v = record(value)
  if (v.kind !== 'node' && v.kind !== 'client') throw new Error('设备类型无效。')
  const member: Member = { device_id: validTarget(v.device_id), kind: v.kind, name: text(v.name), manage: boolean(v.manage) }
  if (member.kind === 'node' && member.manage) throw new Error('设备权限无效。')
  return member
}
function cutoff(value: unknown): number { if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new Error('撤销记录无效。'); return value }
export function parseView(value: unknown): MembershipView {
  const v = record(value)
  return {
    network_id: validTarget(v.network_id), epoch: integer(v.epoch), recovery_key: validTarget(v.recovery_key), heads: strings(v.heads), members: array(v.members).map(parseMember),
    revoked: array(v.revoked).map(item => { const r = record(item); return { device_id: validTarget(r.device_id), cutoff_seq: cutoff(r.cutoff_seq), cutoff_hash: r.cutoff_hash === null ? null : text(r.cutoff_hash) } }),
    attesters: strings(v.attesters),
    pending: v.pending === null ? null : (({ checkpoint, effective_at_ms, members }) => ({ checkpoint: text(checkpoint), effective_at_ms: integer(effective_at_ms), members: array(members).map(parseMember) }))(record(v.pending)),
    stopped: strings(v.stopped),
  }
}
/** The Node's clock in an answer that carries one. */
export const nodeNow = (answer: Record<string, unknown>) => typeof answer.now_ms === 'number' ? answer.now_ms : undefined
export function parsePrincipal(value: unknown): ClientPrincipal {
  const v = record(value)
  const principal = { network_id: validTarget(v.network_id), device_id: validTarget(v.device_id), epoch: integer(v.epoch) }
  if (v.source === 'local_session') {
    if (v.can_manage !== false) throw new Error('本机会话不能取得设备管理权限。')
    return { ...principal, source: 'local_session', can_manage: false }
  }
  if (v.source !== 'membership') throw new Error('设备返回的身份信息认不出来。')
  return { ...principal, source: 'membership', can_manage: boolean(v.can_manage) }
}
const hex32 = (value: unknown) => value === null || value === undefined ? null : validTarget(value)
export function parseJoinRequest(value: unknown): JoinRequest {
  const v = record(value, '加入请求')
  if (!['pending', 'confirming', 'joined', 'rejected', 'withdrawn', 'expired'].includes(v.status as string)) throw new Error('加入状态无效。')
  if (v.kind !== 'node' && v.kind !== 'client') throw new Error('设备类型无效。')
  if (typeof v.invite_id !== 'string' || !/^[a-f0-9]{32}$/.test(v.invite_id)) throw new Error('加入请求无效。')
  return {
    join_id: text(v.join_id), status: v.status as JoinStatus, network_id: validTarget(v.network_id), invite_id: v.invite_id, device_id: validTarget(v.device_id),
    kind: v.kind, name: text(v.name), commit: validTarget(v.commit), admin_id: hex32(v.admin_id), admin_nonce: hex32(v.admin_nonce), joiner_nonce: hex32(v.joiner_nonce),
    reason: typeof v.reason === 'string' ? v.reason : null, expires_at_ms: integer(v.expires_at_ms),
  }
}
export function parseLocalSetup(value: unknown, origin: string): LocalSetup {
  const v = record(value, '本机设置')
  let setup: LocalSetup
  if (v.kind === 'uninitialized') {
    const invitation = record(v.invitation, '本机初始化')
    setup = { kind: 'uninitialized', entry: parseEntry({ address: invitation.url, device_id: invitation.device_id }), capability: validTarget(invitation.capability) }
  } else if (v.kind === 'initialized') {
    const entry = parseEntry(v.entry)
    setup = { kind: 'initialized', entry, removed: boolean(v.removed) }
    if (v.local_launch !== undefined) {
      const launch = record(v.local_launch), url = new URL(text(launch.url)), device_id = validTarget(launch.device_id), token = text(launch.token)
      if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || !url.port || url.username || url.password || url.pathname !== '/' || url.search || url.hash || device_id !== entry.device_id || !/^[a-f0-9]{64}$/.test(token)) throw new Error('本机链接和这台电脑对不上。')
      setup.local_launch = { url: url.origin, device_id, token }
    }
  }
  else throw new Error('本机设置状态无效。')
  if (setup.entry.kind !== 'gateway' || setup.entry.address !== origin) throw new Error('本机设置和当前页面对不上。')
  return setup
}
/** The usual project folders the device found, each named as Settings shows paths, for adding with one tap. */
export function folderSuggestions(response: NodeSettingsResponse): { path: string; label: string }[] {
  return response.suggested_roots.filter((path) => !response.settings.roots.includes(path)).map((path) => ({ path, label: response.display_paths[path] ?? path }))
}

/**
 * Turning a bot on. On a device not set up yet (no bot, no tasks, no folders) it also runs its tasks, so the first work
 * request does not stop at a switch; once someone has set it up, their choice stands.
 */
export function botOnChange(response: NodeSettingsResponse, provider: ProviderStatus): { bot: true; execution?: true } {
  const first = response.settings.manager === null && !response.settings.roots.length && !response.providers.some((item) => item.roles.includes('execution'))
  return first && provider.supports.includes('execution') ? { bot: true, execution: true } : { bot: true }
}

/** Providers in use or on the device get a row; the others fold away until someone looks for one. */
export function providerGroups(providers: readonly ProviderStatus[]): { shown: ProviderStatus[]; folded: ProviderStatus[] } {
  const shown = providers.filter((item) => item.installed || item.roles.length > 0)
  return { shown, folded: providers.filter((item) => !shown.includes(item)) }
}

export function parseNodeSettings(value: unknown): NodeSettingsResponse {
  const v = record(value, '设备设置'), settings = record(v.settings), defaults = record(v.defaults), readiness = record(v.readiness)
  let manager: ManagerSettings | null = null
  if (settings.manager !== null) {
    const m = record(settings.manager)
    if (typeof m.model !== 'string') throw new Error('Bot 设置格式不对。')
    manager = { provider: oneOf(m.provider, providerIds), workspace: text(m.workspace), model: m.model }
  }
  if (readiness.manager !== 'disabled' && readiness.manager !== 'authentication_unverified' && readiness.manager !== 'missing_prerequisites') throw new Error('程序检查结果格式不对。')
  return {
    settings: { roots: strings(settings.roots), manager },
    defaults: { workspace: text(defaults.workspace) },
    readiness: { manager: readiness.manager, missing: array(readiness.missing).map(value => oneOf(value, providerIds)) },
    providers: array(v.providers).map(parseProvider),
    display_paths: Object.fromEntries(Object.entries(record(v.display_paths)).map(([path, shown]) => [path, text(shown)])),
    suggested_roots: strings(v.suggested_roots),
  }
}
export const providerIds: readonly ProviderId[] = ['codex', 'cursor', 'claude', 'pi', 'opencode']
/** A provider named by a peer's capabilities, or null when it names none this client knows. */
export const asProvider = (value: unknown): ProviderId | null => providerIds.includes(value as ProviderId) ? value as ProviderId : null
const providerRoles: ProviderRole[] = ['agent', 'execution']
const providerStates: ProviderStatus['state'][] = ['ready', 'needs_setup', 'starting', 'off']
function oneOf<T extends string>(value: unknown, options: readonly T[]): T {
  if (!options.includes(value as T)) throw new Error('AI 状态格式不对。')
  return value as T
}
function parseIssue(value: unknown): ProviderIssue {
  const v = record(value)
  switch (v.code) {
    case 'bin_missing': return { code: v.code, path: text(v.path) }
    case 'env_missing': return { code: v.code, name: text(v.name) }
    case 'launch_failed': return { code: v.code, detail: text(v.detail) }
    case 'config_invalid': return { code: v.code }
    case 'app_server_not_found': return { code: v.code }
    case 'app_server_unreachable': return { code: v.code, detail: text(v.detail) }
    default: throw new Error('AI 状态格式不对。')
  }
}
/** A model a provider offers; `id` is what the bot's model setting stores. */
export type ModelChoice = { id: string; name: string; default: boolean }
/** `provider_models`: the provider's own list, or `null` when it lists none, so the field stays free text. */
export function parseProviderModels(value: unknown): ModelChoice[] | null {
  const v = record(value, '模型列表')
  if (v.models === null || v.models === undefined) return null
  const models = array(v.models).map((item) => { const model = record(item, '模型'); return { id: text(model.id), name: text(model.name), default: model.default === true } })
  return models.length ? models : null
}
/** Whether `model` is typed rather than picked: it is set and the list does not have it. */
export const modelTyped = (model: string, models: ModelChoice[] | null) => Boolean(model) && !models?.some((choice) => choice.id === model)
export function parseAppServerLink(value: unknown): AppServerLink {
  const v = record(value, 'App Server 状态')
  switch (v.state) {
    case 'off': case 'starting': return { state: v.state }
    case 'connected': return { state: v.state, socket: text(v.socket), user_agent: text(v.user_agent) }
    case 'not_found': return { state: v.state, candidates: strings(v.candidates) }
    case 'handshake_failed': return { state: v.state, socket: text(v.socket), reason: text(v.reason) }
    case 'launch_failed': return { state: v.state, reason: text(v.reason) }
    default: throw new Error('App Server 状态格式不对。')
  }
}
function parseAppServer(value: unknown): AppServer | null {
  if (value === null || value === undefined) return null
  const v = record(value, 'App Server 设置')
  return { mode: oneOf(v.mode, ['system', 'spawn'] as const), socket: v.socket === null ? null : text(v.socket), link: parseAppServerLink(v.link) }
}
function parseProvider(value: unknown): ProviderStatus {
  const v = record(value)
  return {
    provider: oneOf(v.provider, providerIds),
    roles: array(v.roles).map(role => oneOf(role, providerRoles)), supports: array(v.supports).map(role => oneOf(role, providerRoles)),
    installed: boolean(v.installed), bin: text(v.bin), app_server: parseAppServer(v.app_server),
    home: typeof v.home === 'string' ? v.home : null, sessions: typeof v.sessions === 'string' ? v.sessions : null,
    env: array(v.env).map(row => { const r = record(row); return { name: text(r.name), set: boolean(r.set), inherited: boolean(r.inherited), required: boolean(r.required) } }),
    state: oneOf(v.state, providerStates), issues: array(v.issues).map(parseIssue),
  }
}
function canonical(value: unknown): string {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value)
  if (typeof value === 'number' && Number.isFinite(value)) return JSON.stringify(value)
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`
  if (isRecord(value)) return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`
  throw new Error('待确认的内容格式不对。')
}
export function same(left: unknown, right: unknown): boolean { return canonical(left) === canonical(right) }
export const messageOf = (error: unknown, action?: string) => error instanceof Error ? humanError(error.message, (error as { code?: unknown }).code, action) : humanError(String(error), undefined, action)
export function deviceName(value: string): string {
  const name = value.trim()
  if (!name || new TextEncoder().encode(name).length > 128 || /[\u0000-\u001f\u007f]/.test(name)) throw new Error('设备名称需要 1 至 128 字节，不能包含控制字符。')
  return name
}
