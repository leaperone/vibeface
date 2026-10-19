import { Identity, recoveryOpen, recoverySeal, verifyMembership } from '../../generated/vibeface_secure.js'
import { integer, parsePrincipal, parseView, record, same, text } from './node'
import type { Member, MembershipView, PublicIdentity } from './node'

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })
export { Identity, verifyMembership }
export function verifyState(state: unknown, clusterId: string, owner: string) {
  return verifyMembership(JSON.stringify(state), clusterId, owner)
}
export function verifiedView(state: unknown, clusterId: string, owner: string) {
  const verified = verifyState(state, clusterId, owner)
  try { return parseView(JSON.parse(verified.viewJson())) } finally { verified.free() }
}
export function mergeMembership(state: unknown, clusterId: string, known: unknown) {
  const owner = text(record(record(state).genesis).owner_public_key)
  const previous = known ? verifyState(known, clusterId, owner) : null
  let verified: ReturnType<typeof verifyState> | undefined
  try {
    verified = previous ? previous.mergeStateJson(JSON.stringify(state)) : verifyState(state, clusterId, owner)
    return { state: JSON.parse(verified.stateJson()), view: parseView(JSON.parse(verified.viewJson())) }
  } finally { verified?.free(); previous?.free() }
}
export class MembershipRevokedError extends Error {}
export function authorizeMembership(view: MembershipView, own: PublicIdentity, actual?: PublicIdentity) {
  if (view.revoked.includes(own.peerId)) throw new MembershipRevokedError('此浏览器的集群授权已被签名撤销。请重新配对。')
  const member = view.members.find(item => item.subject_id === own.peerId)
  if (!member || member.kind !== 'client' || member.public_key !== own.publicKey) throw new Error('此入口尚未确认浏览器的集群授权。')
  if (actual) {
    const gateway = view.members.find(item => item.subject_id === actual.peerId)
    if (!gateway || gateway.kind !== 'node' || gateway.public_key !== actual.publicKey || view.revoked.includes(actual.peerId) || !gateway.node_roles.includes('gateway')) throw new Error('入口不是当前已授权的集群 Gateway。')
  }
  return member
}
export type AuthenticationTrust = { kind: 'cluster'; clusterId: string } | { kind: 'root'; clusterId: string; ownerPublicKey: string; genesis: unknown; checkpoint?: unknown } | { kind: 'local'; clusterId: string; peerId: string; nodePublicKey: string }
export function verifyAuthentication(value: unknown, own: PublicIdentity, expected: AuthenticationTrust | null, effective?: { view: MembershipView; actual: PublicIdentity }) {
  const auth = record(value)
  const principal = parsePrincipal(auth.principal)
  const state = record(auth.state)
  if (principal.subject_id !== own.peerId || expected && principal.cluster_id !== expected.clusterId) throw new Error('认证返回的集群或浏览器身份不一致。')
  if (expected?.kind === 'root' && (!same(state.genesis, expected.genesis) || expected.checkpoint !== undefined && !same(state.checkpoint, expected.checkpoint))) throw new Error('认证返回的根授权与本地确认的记录不一致。')
  const owner = expected?.kind === 'root' ? expected.ownerPublicKey : text(record(state.genesis).owner_public_key)
  const reportedView = verifiedView(state, expected?.clusterId ?? principal.cluster_id, owner)
  if (principal.epoch !== reportedView.epoch || !same(auth.view, reportedView)) throw new Error('认证目录与已签名的成员记录不一致。')
  const view = effective?.view ?? reportedView
  if (principal.source === 'local_session') {
    const node = expected?.kind === 'local' ? view.members.find(member => member.subject_id === expected.peerId) : undefined
    if (expected?.kind !== 'local' || !node || node.kind !== 'node' || node.public_key !== expected.nodePublicKey || view.revoked.includes(node.subject_id) || principal.can_manage) throw new Error('本机会话未绑定当前已验证的 Node。')
    return { auth, principal, view }
  }
  const reportedMember = reportedView.members.find(item => item.subject_id === own.peerId)
  const member = authorizeMembership(view, own, effective?.actual)
  if (!reportedMember || reportedView.revoked.includes(own.peerId) || reportedMember.public_key !== own.publicKey || reportedMember.kind !== 'client' || principal.epoch !== view.epoch || principal.can_manage !== reportedMember.manage || principal.can_manage && !member.manage) throw new Error('设备授权与已签名的成员记录不一致。')
  return { auth, principal, view }
}
export function verifyRecoveryResult(value: unknown, expected: { clusterId: string; ownerPublicKey: string; genesis: unknown; checkpoint: unknown; manager: Member }) {
  const epoch = integer(record(expected.checkpoint).epoch)
  const { auth, principal, view } = verifyAuthentication(value, { peerId: expected.manager.subject_id, publicKey: expected.manager.public_key }, { kind: 'root', ...expected })
  const manager = view.members.find(member => member.subject_id === expected.manager.subject_id)
  if (principal.epoch !== epoch || !principal.can_manage || !manager || !same(manager, expected.manager)) throw new Error('恢复响应的管理身份或授权版本不一致，未生成新的恢复包。')
  return { auth, epoch }
}
export function passphrase(password: string, confirmation: string): string {
  if ([...password].length < 12 || encoder.encode(password).length > 1024) throw new Error('恢复包口令需要至少 12 个字符，且不超过 1024 字节。')
  if (password !== confirmation) throw new Error('两次口令不一致。')
  return password
}
export function sealPackage(payload: Record<string, unknown>, password: string): Uint8Array {
  const cleartext = encoder.encode(JSON.stringify(payload))
  try {
    const envelope: unknown = JSON.parse(decoder.decode(recoverySeal(cleartext, password)))
    return encoder.encode(JSON.stringify({ version: 2, cluster_id: payload.cluster_id, recovery_epoch: payload.recovery_epoch, envelope }))
  } finally { cleartext.fill(0) }
}
export async function openPackage(file: File, password: string) {
  if (!file.size || file.size > 256 * 1024) throw new Error('选择不超过 256 KiB 的加密恢复包。')
  const bundle = record(JSON.parse(await file.text()), '恢复包')
  if (bundle.version !== 2) throw new Error('恢复包版本无效。')
  const epoch = integer(bundle.recovery_epoch)
  const clusterId = text(bundle.cluster_id)
  let cleartext: Uint8Array
  try { cleartext = recoveryOpen(encoder.encode(JSON.stringify(record(bundle.envelope))), password) }
  catch { throw new Error('无法解锁恢复包。请核对口令，并使用未修改的原始文件。') }
  let payload: Record<string, unknown>
  try { payload = record(JSON.parse(decoder.decode(cleartext)), '恢复内容') } finally { cleartext.fill(0) }
  let root: Identity | undefined
  try {
    root = Identity.fromSecretHex(text(payload.root_secret_key))
    if (payload.version !== 2 || payload.cluster_id !== clusterId || payload.recovery_epoch !== epoch || root.peerId() !== clusterId || !Array.isArray(payload.decryption_keys)) throw new Error('恢复包身份与加密内容不一致。')
    const state = { genesis: record(payload.genesis), checkpoint: null, events: [] }
    const verified = verifyState(state, clusterId, root.publicKeyHex())
    verified.free()
    const openedRoot = root
    let released = false
    return { payload, root: openedRoot, clusterId, epoch, release() { if (!released) { released = true; openedRoot.free(); payload.root_secret_key = '' } } }
  } catch (error) { root?.free(); payload.root_secret_key = ''; throw error }
}
export function downloadPackage(bytes: Uint8Array, clusterId: string) {
  const copy = new Uint8Array(bytes)
  const url = URL.createObjectURL(new Blob([copy.buffer], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = `vibeface-recovery-${clusterId.slice(0, 12)}.json`
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
