import { sha256Hex } from '../lib/byte-codec.ts'
import { array, canonical, deviceName, entryJson, isSafeInteger, parseEntry, parseMember, record, same, text } from '../lib/node.ts'
import type { ConnectionEntry, JoinRequest, Member, MembershipView } from '../lib/node.ts'
import type { KeyValue } from '../lib/platform.ts'
import { validTarget } from '../lib/transport.ts'
import type { SecureApi } from '../secure-api.ts'

type JoinPin = { network_id: string; admin_id: string; admin_nonce: string }
/** The joiner's own nonce and, once fixed, the manager's commitment before the joiner reveals. */
export type PendingJoin = { entry: ConnectionEntry; join_id: string; invite_id: string; nonce: string; name: string; pinned?: JoinPin }
/** A saved intent is checking until the first matching status arrives from the Node. */
export type Joining = PendingJoin & { status: 'checking' | JoinRequest['status']; code?: string; admin?: string }
export type ApprovalPin = { nonce: string; device_id: string; invite_id: string; commit: string }
type ApprovalPins = { pins: Map<string, ApprovalPin>; damaged: boolean }

type Genesis = { version: 1; network_id: string; first_client: Member; first_node: Member; signature: string }
type Revocation = MembershipView['revoked'][number]
type RecoveryCheckpoint = {
  version: 1
  network_id: string
  epoch: number
  recovery_key: string
  directory: { members: Member[]; retired: Revocation[] }
  heads: string[]
  issued_at_ms: number
  signature: string
}
/** A recovery submitted from here. A wait picked up from the Node may have no local copy of its checkpoint. */
export type WaitRecord = { entry: ConnectionEntry; networkId: string; genesis: Genesis; rootKey: string; checkpoint?: RecoveryCheckpoint }

export type IdentityRecord<T> = { kind: 'empty' } | { kind: 'unreadable' } | { kind: 'invalid' } | { kind: 'valid'; value: T }

export function readIdentityRecord<T>(storage: Pick<KeyValue, 'getItem'>, key: string, parse: (value: unknown) => T): IdentityRecord<T> {
  let raw: string | null
  try { raw = storage.getItem(`vibeface.${key}`) } catch { return { kind: 'unreadable' } }
  if (raw === null) return { kind: 'empty' }
  try {
    const value: unknown = JSON.parse(raw)
    return value === null ? { kind: 'empty' } : { kind: 'valid', value: parse(value) }
  } catch { return { kind: 'invalid' } }
}

const invalid = () => new Error('本地身份记录格式无效。')

function exactRecord(value: unknown, fields: readonly string[], optional?: string) {
  const v = record(value, '本地身份记录')
  if (fields.some(key => !Object.hasOwn(v, key)) || Object.keys(v).some(key => !fields.includes(key) && key !== optional)) throw invalid()
  return v
}

function connectionEntry(value: unknown): ConnectionEntry {
  const entry = parseEntry(value)
  if (!same(value, entryJson(entry))) throw invalid()
  return entry
}

function inviteId(value: unknown): string {
  const id = text(value)
  if (!/^[a-f0-9]{32}$/.test(id)) throw invalid()
  return id
}

export function parsePendingJoin(value: unknown): PendingJoin {
  const v = exactRecord(value, ['entry', 'join_id', 'invite_id', 'nonce', 'name'], 'pinned')
  const join_id = inviteId(v.join_id), invite_id = inviteId(v.invite_id)
  const name = text(v.name)
  if (join_id !== invite_id || deviceName(name) !== name) throw invalid()
  const pending: PendingJoin = { entry: connectionEntry(v.entry), join_id, invite_id, nonce: validTarget(v.nonce), name }
  if (v.pinned === undefined) return pending
  const pin = exactRecord(v.pinned, ['network_id', 'admin_id', 'admin_nonce'])
  return { ...pending, pinned: { network_id: validTarget(pin.network_id), admin_id: validTarget(pin.admin_id), admin_nonce: validTarget(pin.admin_nonce) } }
}

export function pendingJoinJson({ entry, join_id, invite_id, nonce, name, pinned }: PendingJoin) {
  return { entry: entryJson(entry), join_id, invite_id, nonce, name, ...(pinned === undefined ? {} : { pinned }) }
}

function parseApprovalPin(value: unknown, joinId: string): ApprovalPin {
  const v = exactRecord(value, ['nonce', 'device_id', 'invite_id', 'commit'])
  const invite_id = inviteId(v.invite_id)
  if (invite_id !== joinId) throw invalid()
  return { nonce: validTarget(v.nonce), device_id: validTarget(v.device_id), invite_id, commit: validTarget(v.commit) }
}

export function parseApprovalPins(value: unknown): ApprovalPins {
  const pins = new Map<string, ApprovalPin>()
  let damaged = false
  for (const [joinId, pin] of Object.entries(record(value, '本地审批记录'))) {
    try { pins.set(joinId, parseApprovalPin(pin, joinId)) } catch { damaged = true }
  }
  return { pins, damaged }
}

export function approvalMatches(request: JoinRequest, networkId: string, pin: Pick<ApprovalPin, 'device_id' | 'invite_id' | 'commit'>): boolean {
  return request.join_id === request.invite_id && request.network_id === networkId && request.device_id === pin.device_id && request.invite_id === pin.invite_id && request.commit === pin.commit
}

export function joinMatches(request: JoinRequest, pending: PendingJoin, deviceId: string, commit: string): boolean {
  return request.join_id === pending.join_id && request.invite_id === pending.invite_id && request.device_id === deviceId && request.kind === 'client' && request.commit === commit && (request.joiner_nonce === null || request.joiner_nonce === pending.nonce)
}

function signature(value: unknown): string {
  const signed = text(value)
  if (!/^[a-fA-F0-9]{128}$/.test(signed)) throw invalid()
  return signed
}

function signedMember(value: unknown): Member {
  const v = exactRecord(value, ['device_id', 'kind', 'name', 'manage'])
  const member = parseMember(v)
  // Rust str::trim uses Unicode White_Space. ECMAScript trim also removes U+FEFF.
  if (!/\P{White_Space}/u.test(member.name) || new TextEncoder().encode(member.name).length > 128 || /[\p{Cc}\p{Cs}]/u.test(member.name)) throw invalid()
  return { ...v, ...member }
}

function parseGenesis(value: unknown, networkId: string): Genesis {
  const v = exactRecord(value, ['version', 'network_id', 'first_client', 'first_node', 'signature'])
  if (v.version !== 1 || v.network_id !== networkId) throw invalid()
  return { ...v, version: v.version, network_id: networkId, first_client: signedMember(v.first_client), first_node: signedMember(v.first_node), signature: signature(v.signature) }
}

function parseRevocation(value: unknown): Revocation {
  const v = exactRecord(value, ['device_id', 'cutoff_seq', 'cutoff_hash'])
  const cutoff_seq = v.cutoff_seq
  if (!isSafeInteger(cutoff_seq) || cutoff_seq < 0 || (cutoff_seq === 0) !== (v.cutoff_hash === null)) throw invalid()
  return { ...v, device_id: validTarget(v.device_id), cutoff_seq, cutoff_hash: v.cutoff_hash === null ? null : validTarget(v.cutoff_hash) }
}

type WaitTrust = { deviceId: string; verifyGenesis(genesis: Genesis, networkId: string): void; verify: SecureApi['verify'] }

function strictlySorted(ids: string[]): boolean { return ids.every((id, index) => index === 0 || ids[index - 1] < id) }

function parseRecoveryCheckpoint(value: unknown, { entry, networkId, rootKey }: Omit<WaitRecord, 'checkpoint'>, trust: WaitTrust): RecoveryCheckpoint {
  const v = exactRecord(value, ['version', 'network_id', 'epoch', 'recovery_key', 'directory', 'heads', 'issued_at_ms', 'signature'])
  const { epoch, issued_at_ms } = v
  if (v.version !== 1 || v.network_id !== networkId || !isSafeInteger(epoch) || epoch < 2 || !isSafeInteger(issued_at_ms) || issued_at_ms < 0) throw invalid()
  const recovery_key = validTarget(v.recovery_key)
  const directory = exactRecord(v.directory, ['members', 'retired'])
  const members = array(directory.members).map(signedMember), retired = array(directory.retired).map(parseRevocation)
  const ids = members.map(member => member.device_id), included = new Set(ids)
  const retiredIds = retired.map(member => member.device_id)
  const heads = array(v.heads).map(validTarget)
  if (epoch === 2 && rootKey !== networkId || rootKey === recovery_key || included.has(rootKey) || included.has(recovery_key)
    || members.length > 1024 || retired.length > 1024 || heads.length > 64 * 1025
    || !strictlySorted(ids) || !strictlySorted(retiredIds) || retiredIds.some(id => included.has(id)) || !strictlySorted(heads)
    || !members.some(member => member.device_id === trust.deviceId && member.kind === 'client' && member.manage)
    || !members.some(member => member.device_id === entry.device_id && member.kind === 'node')) throw invalid()
  const checkpoint: RecoveryCheckpoint = { ...v, version: v.version, network_id: networkId, epoch, recovery_key, directory: { ...directory, members, retired }, heads, issued_at_ms, signature: signature(v.signature) }
  // The earlier chain is on the Node. Here we can validate the exact bytes against the root recorded for this attempt.
  trust.verify(rootKey, 'identity-recovery', new TextEncoder().encode(canonical({ ...checkpoint, signature: '' })), checkpoint.signature)
  return checkpoint
}

export function parseWaitRecord(value: unknown, trust: WaitTrust): WaitRecord {
  const v = exactRecord(value, ['entry', 'networkId', 'genesis', 'rootKey'], 'checkpoint')
  const networkId = validTarget(v.networkId)
  const genesis = parseGenesis(v.genesis, networkId)
  trust.verifyGenesis(genesis, networkId)
  const entry = connectionEntry(v.entry), rootKey = validTarget(v.rootKey)
  if (rootKey === trust.deviceId || rootKey === entry.device_id) throw invalid()
  const wait = { entry, networkId, genesis, rootKey }
  return v.checkpoint === undefined ? wait : { ...wait, checkpoint: parseRecoveryCheckpoint(v.checkpoint, wait, trust) }
}

export function waitRecordJson({ entry, networkId, genesis, rootKey, checkpoint }: WaitRecord) {
  return { entry: entryJson(entry), networkId, genesis, rootKey, ...(checkpoint === undefined ? {} : { checkpoint }) }
}

export function recoveryCheckpointHash(checkpoint: RecoveryCheckpoint): Promise<string> {
  return sha256Hex(new TextEncoder().encode(canonical(checkpoint)))
}
