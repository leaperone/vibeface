import type { IdentityScope } from './scope'
import { validEndpoint, validTarget } from '../../transport.js'

export type ConnectionEntry = { address: string; node_id: string; relay: boolean }
export type PublicIdentity = { nodeId: string; publicKey: string; profileId?: string | null }
export type ClientPrincipal = { network_id: string; subject_id: string; epoch: number; can_manage: boolean }
export type NodeRole = 'gateway' | 'relay' | 'replica' | 'scheduler' | 'executor'
export type EntryInfo = { node_id: string; public_key: string; version: string; roles: NodeRole[]; initialized: boolean }
export type ReadyConnection = {
  readonly generation: number
  readonly scope: IdentityScope
  readonly audience: { device: string; service: string }
  entry: ConnectionEntry
  authorization: { enrollmentRights: string[]; actions: string[]; authorityEpoch: number }
  info: EntryInfo
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  rpc(method: string, params?: Record<string, unknown>): Promise<unknown>
  subscribe(listener: () => void): () => void
  isCurrent(): boolean
}
export type Member = { subject_id: string; public_key: string; kind: 'node' | 'client'; name: string; node_roles: NodeRole[]; manage: boolean }
export type MembershipView = { network_id: string; epoch: number; heads: string[]; members: Member[]; revoked: string[] }
export type PairReview = { version: number; invite_id: string; network_id: string; inviter_subject_id: string; new_member: Member; proposal_hash: string; parents: string[]; epoch: number; expires_at_ms: number }
export type PairReceipt = { request_id: string; review: PairReview; new_subject_signature: string }
export type Signing = { domain: string; payload: string }
export type WaitingPair = { request_id: string; network_id: string; entry: ConnectionEntry; review: PairReview; signing: Signing; accepted: boolean }
export type Invitation = { kind: 'bootstrap'; entry: ConnectionEntry; capability: string } | { kind: 'pair'; entry: ConnectionEntry; invite: Record<string, unknown>; capability: string; payload: Record<string, unknown> }

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
export const entryKey = (entry: ConnectionEntry) => `${entry.address}:${entry.node_id}:${entry.relay}`
export function parseInfo(value: unknown): EntryInfo {
  const v = record(value)
  return { node_id: validTarget(v.node_id), public_key: validTarget(v.public_key), version: text(v.version), roles: roles(v.roles), initialized: boolean(v.initialized) }
}
export function parseMember(value: unknown): Member {
  const v = record(value)
  if (v.kind !== 'node' && v.kind !== 'client') throw new Error('设备类型无效。')
  const member: Member = { subject_id: validTarget(v.subject_id), public_key: validTarget(v.public_key), kind: v.kind, name: text(v.name), node_roles: roles(v.node_roles), manage: boolean(v.manage) }
  if ((member.kind === 'node' && member.manage) || (member.kind === 'client' && member.node_roles.length)) throw new Error('设备权限无效。')
  return member
}
export function parseView(value: unknown): MembershipView {
  const v = record(value)
  return { network_id: validTarget(v.network_id), epoch: integer(v.epoch), heads: strings(v.heads), members: array(v.members).map(parseMember), revoked: strings(v.revoked) }
}
export function parsePrincipal(value: unknown): ClientPrincipal {
  const v = record(value)
  return { network_id: validTarget(v.network_id), subject_id: validTarget(v.subject_id), epoch: integer(v.epoch), can_manage: boolean(v.can_manage) }
}
export function parseReview(value: unknown): PairReview {
  const v = record(value)
  const review = { version: integer(v.version), invite_id: text(v.invite_id), network_id: validTarget(v.network_id), inviter_subject_id: validTarget(v.inviter_subject_id), new_member: parseMember(v.new_member), proposal_hash: validTarget(v.proposal_hash), parents: strings(v.parents), epoch: integer(v.epoch), expires_at_ms: integer(v.expires_at_ms) }
  if (review.version !== 1 || review.new_member.manage) throw new Error('普通配对不能授予管理权限。')
  return review
}
export function parseSigning(value: unknown): Signing { const v = record(value); return { domain: text(v.domain), payload: text(v.payload) } }
export function parsePairReceipt(value: unknown): PairReceipt {
  const v = record(value)
  if (typeof v.new_subject_signature !== 'string') throw new Error('配对确认签名无效。')
  return { request_id: text(v.request_id), review: parseReview(v.review), new_subject_signature: v.new_subject_signature }
}
export function parseInvitation(value: unknown): Invitation {
  const v = record(value, '邀请')
  const entry = parseEntry({ address: v.url, node_id: v.node_id, relay: v.relay ?? false })
  if (typeof v.capability === 'string' && !v.invite && !v.payload) return { kind: 'bootstrap', entry, capability: text(v.capability) }
  const envelope = record(v.invite, '配对邀请')
  const invite = record(envelope.invite, '配对授权')
  const payload = record(invite.payload, '邀请权限')
  if (payload.new_kind !== 'client') throw new Error('这是运行节点邀请。请在目标电脑上运行 vibeface pair。')
  if (payload.manage !== false || roles(payload.node_roles).length || integer(payload.expires_at_ms) <= Date.now()) throw new Error('邀请已过期或权限无效。请创建新邀请。')
  validTarget(payload.network_id)
  return { kind: 'pair', entry, invite, capability: text(envelope.capability), payload }
}
export function canonical(value: unknown): string {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value)
  if (typeof value === 'number' && Number.isFinite(value)) return JSON.stringify(value)
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`
  if (isRecord(value)) return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`
  throw new Error('签名内容包含无效的数据。')
}
export function same(left: unknown, right: unknown): boolean { return canonical(left) === canonical(right) }
export function signingPayload(value: unknown, domain: string, expected: unknown): string {
  const signing = parseSigning(value)
  if (signing.domain !== domain || signing.payload.length > 262144 || signing.payload !== canonical(expected)) throw new Error('签名内容与正在确认的设备或权限不一致，已停止授权。')
  return signing.payload
}
export async function digest(value: unknown): Promise<string> {
  const bytes = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(canonical(value)))
  return [...new Uint8Array(bytes)].map(byte => byte.toString(16).padStart(2, '0')).join('')
}
export const messageOf = (error: unknown) => error instanceof Error ? error.message : String(error)
export function deviceName(value: string): string {
  const name = value.trim()
  if (!name || new TextEncoder().encode(name).length > 128 || /[\u0000-\u001f\u007f]/.test(name)) throw new Error('设备名称需要 1 至 128 字节，不能包含控制字符。')
  return name
}
