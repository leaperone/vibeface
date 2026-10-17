import type { IdentityScope } from './scope'
import { validEndpoint, validTarget } from '../../transport.js'

export type ConnectionEntry = { address: string; node_id: string; relay: boolean }
export type ConnectionCard = { kind: 'connection'; entry: ConnectionEntry; nodePublicKey: string } | { kind: 'claim'; entry: ConnectionEntry; nodePublicKey: string; capability: string; claimNonce: string }
export type PublicIdentity = { nodeId: string; publicKey: string; profileId?: string | null }
export type NodeRole = 'gateway' | 'relay' | 'replica' | 'scheduler' | 'executor'
export type EntryInfo = { node_id: string; public_key: string; version: string; roles: NodeRole[]; initialized: boolean }
export type ProjectResource = Readonly<{ id: string; host: string }>
export type AccountDevice = { device_id: string; public_key: string; subject: { kind: 'account' | 'service'; id: string }; uses: string[]; not_before_ms: number; expires_at_ms: number; status: 'active' | 'expired' | 'pending' }
export type AccountDeviceDirectory = { account_id: string; devices: AccountDevice[] }
export type AuthorizedRequest = { body: Record<string, unknown>; invocation: unknown }
export type AuthorizedReply = AuthorizedRequest & { result: unknown }
export type RpcFacts = {
  taskRef?: string
  execution?: { sessionId: string; executorKind: string; canonicalCwd: string; bindingGeneration?: number }
}
export type ReadyConnection = {
  readonly generation: number
  readonly scope: IdentityScope
  readonly audience: { device: string; service: string }
  readonly project: ProjectResource
  readonly projects: ProjectResource[]
  readonly audiencePublicKey: string
  entry: ConnectionEntry
  authorization: { enrollmentRights: string[]; actions: string[]; authorityEpoch: number }
  info: EntryInfo
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  rpc(method: string, params?: Record<string, unknown>, facts?: RpcFacts): Promise<unknown>
  rpcWithProof(method: string, params: Record<string, unknown>, beforeSend?: (proof: AuthorizedRequest) => void, facts?: RpcFacts): Promise<AuthorizedReply>
  subscribe(listener: () => void): () => void
  isCurrent(): boolean
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
export function integer(value: unknown): number { if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) throw new Error('设备返回的编号或版本无效。'); return value }
export function strings(value: unknown): string[] { if (!Array.isArray(value)) throw new Error('设备返回的列表格式无效。'); return value.map(item => text(item)) }
export function array(value: unknown): unknown[] { if (!Array.isArray(value)) throw new Error('设备返回的列表格式无效。'); return value }
export function roles(value: unknown): NodeRole[] {
  return strings(value).map(role => { switch (role) { case 'gateway': case 'relay': case 'replica': case 'scheduler': case 'executor': return role; default: throw new Error('设备返回了不支持的角色。') } })
}
export function parseEntry(value: unknown): ConnectionEntry {
  const v = record(value, '连接入口')
  return { address: validEndpoint(text(v.address)), node_id: validTarget(v.node_id), relay: boolean(v.relay) }
}
export function parseConnectionCard(value: unknown): ConnectionCard {
  const card = record(value, '连接卡')
  if (Object.keys(card).some(key => !['url', 'node_id', 'relay', 'node_public_key', 'capability', 'claim_nonce'].includes(key))) throw new Error('连接卡包含不支持的旧授权或额外字段。请重新运行 vibeface access。')
  const entry = parseEntry({ address: card.url, node_id: card.node_id, relay: card.relay ?? false })
  const nodePublicKey = validTarget(card.node_public_key)
  if (card.capability === undefined && card.claim_nonce === undefined) return { kind: 'connection', entry, nodePublicKey }
  const capability = validTarget(card.capability)
  const claimNonce = validTarget(card.claim_nonce)
  if (capability !== claimNonce) throw new Error('连接卡的认领凭证与挑战随机数不一致。')
  return { kind: 'claim', entry, nodePublicKey, capability, claimNonce }
}
export const entryKey = (entry: ConnectionEntry) => `${entry.address}:${entry.node_id}:${entry.relay}`
export function parseInfo(value: unknown): EntryInfo {
  const v = record(value)
  return { node_id: validTarget(v.node_id), public_key: validTarget(v.public_key), version: text(v.version), roles: roles(v.roles), initialized: boolean(v.initialized) }
}
export function canonical(value: unknown): string {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value)
  if (typeof value === 'number' && Number.isFinite(value)) return JSON.stringify(value)
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`
  if (isRecord(value)) return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`
  throw new Error('签名内容包含无效的数据。')
}
export function same(left: unknown, right: unknown): boolean { return canonical(left) === canonical(right) }
export const messageOf = (error: unknown) => error instanceof Error ? error.message : String(error)
