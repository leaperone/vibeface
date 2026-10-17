import { Identity, recoveryOpen, recoverySeal, verifyMembership } from '../../generated/vibeface_secure.js'
import { integer, parsePrincipal, parseView, record, same, text } from './node'
import type { Member, PublicIdentity } from './node'

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })
export { Identity, verifyMembership }
export function verifyState(state: unknown, networkId: string, owner: string) {
  return verifyMembership(JSON.stringify(state), networkId, owner)
}
export function verifiedView(state: unknown, networkId: string, owner: string) {
  const verified = verifyState(state, networkId, owner)
  try { return parseView(JSON.parse(verified.viewJson())) } finally { verified.free() }
}
export type AuthenticationTrust = { kind: 'network'; networkId: string } | { kind: 'root'; networkId: string; ownerPublicKey: string; genesis: unknown; checkpoint?: unknown }
export function verifyAuthentication(value: unknown, own: PublicIdentity, expected: AuthenticationTrust | null) {
  const auth = record(value)
  const principal = parsePrincipal(auth.principal)
  const state = record(auth.state)
  if (principal.subject_id !== own.nodeId || expected && principal.network_id !== expected.networkId) throw new Error('认证返回的个人网络或浏览器身份不一致。')
  if (expected?.kind === 'root' && (!same(state.genesis, expected.genesis) || expected.checkpoint !== undefined && !same(state.checkpoint, expected.checkpoint))) throw new Error('认证返回的根授权与本地确认的记录不一致。')
  const owner = expected?.kind === 'root' ? expected.ownerPublicKey : text(record(state.genesis).owner_public_key)
  const view = verifiedView(state, expected?.networkId ?? principal.network_id, owner)
  const member = view.members.find(item => item.subject_id === own.nodeId)
  if (!member || view.revoked.includes(own.nodeId) || member.public_key !== own.publicKey || member.kind !== 'client' || principal.epoch !== view.epoch || principal.can_manage !== member.manage || !same(auth.view, view)) throw new Error('设备授权与已签名的成员记录不一致。')
  return { auth, principal, view }
}
export function verifyRecoveryResult(value: unknown, expected: { networkId: string; ownerPublicKey: string; genesis: unknown; checkpoint: unknown; manager: Member }) {
  const epoch = integer(record(expected.checkpoint).epoch)
  const { auth, principal, view } = verifyAuthentication(value, { nodeId: expected.manager.subject_id, publicKey: expected.manager.public_key }, { kind: 'root', ...expected })
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
    return encoder.encode(JSON.stringify({ version: 1, network_id: payload.network_id, recovery_epoch: payload.recovery_epoch, envelope }))
  } finally { cleartext.fill(0) }
}
export async function openPackage(file: File, password: string) {
  if (!file.size || file.size > 256 * 1024) throw new Error('选择不超过 256 KiB 的加密恢复包。')
  const bundle = record(JSON.parse(await file.text()), '恢复包')
  if (bundle.version !== 1) throw new Error('恢复包版本无效。')
  const epoch = integer(bundle.recovery_epoch)
  const networkId = text(bundle.network_id)
  let cleartext: Uint8Array
  try { cleartext = recoveryOpen(encoder.encode(JSON.stringify(record(bundle.envelope))), password) }
  catch { throw new Error('无法解锁恢复包。请核对口令，并使用未修改的原始文件。') }
  let payload: Record<string, unknown>
  try { payload = record(JSON.parse(decoder.decode(cleartext)), '恢复内容') } finally { cleartext.fill(0) }
  let root: Identity | undefined
  try {
    root = Identity.fromSecretHex(text(payload.root_secret_key))
    if (payload.version !== 1 || payload.network_id !== networkId || payload.recovery_epoch !== epoch || root.nodeId() !== networkId || !Array.isArray(payload.decryption_keys)) throw new Error('恢复包身份与加密内容不一致。')
    const state = { genesis: record(payload.genesis), checkpoint: null, events: [] }
    const verified = verifyState(state, networkId, root.publicKeyHex())
    verified.free()
    const openedRoot = root
    let released = false
    return { payload, root: openedRoot, networkId, epoch, release() { if (!released) { released = true; openedRoot.free(); payload.root_secret_key = '' } } }
  } catch (error) { root?.free(); payload.root_secret_key = ''; throw error }
}
export function downloadPackage(bytes: Uint8Array, networkId: string) {
  const copy = new Uint8Array(bytes)
  const url = URL.createObjectURL(new Blob([copy.buffer], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = `vibeface-recovery-${networkId.slice(0, 12)}.json`
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
