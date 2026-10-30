import type { SecureApi, SecureIdentity } from '../secure-api.ts'
import { array, nodeNow, parsePrincipal, parseView, record, same, text } from './node.ts'
import type { ClientPrincipal, MembershipView, PublicIdentity } from './node.ts'

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })
const notRecoveryFile = '这不是恢复文件。请选名字以 vibeface-recovery 开头的文件。'
const damagedRecoveryFile = '这个恢复文件已损坏。请选下载时的那个文件。'
/** What the device already trusts before it reads an authentication answer. */
export type AuthenticationTrust = { kind: 'network'; networkId: string } | { kind: 'root'; networkId: string; genesis: unknown; checkpoint?: unknown } | { kind: 'local'; networkId: string; nodeId: string }
/** A recovery file as the platform hands it over: a browser `File` or a phone's picked document. */
export type RecoveryFile = { size: number; text(): Promise<string> }
/** What a recovery file carries besides its key: the whole checkpoint chain and the management events it rests on (the heads each recovery names, and the vetoes), so an empty Node can check it. */
type RecoveryMaterial = { networkId: string; genesis: unknown; checkpoints: unknown[]; events: unknown[] }

/** The membership and recovery checks built on `crates/secure`, for whichever build of it the platform loaded. */
export function secureKit(secure: SecureApi) {
  const { Identity, createGenesis, governancePayload, inviteSigningPayload, joinCommit, shortCode, ticketEncode, attestationPayload } = secure
  /** An entrance record counts only when its own Node signed these exact bytes. */
  function verifyAddress(deviceId: string, payload: string, signature: string): boolean {
    try { secure.verify(deviceId, 'address', encoder.encode(payload), signature); return true } catch { return false }
  }
  function verifyState(state: unknown, networkId: string, now?: number) { return secure.verifyMembership(JSON.stringify(state), networkId, now) }
  function verifiedView(state: unknown, networkId: string, now?: number): MembershipView {
    const verified = verifyState(state, networkId, now)
    try { return parseView(JSON.parse(verified.viewJson())) } finally { verified.free() }
  }
  function verifyAuthentication(value: unknown, own: PublicIdentity, expected: AuthenticationTrust | null): { auth: Record<string, unknown>; principal: ClientPrincipal; view: MembershipView } {
    const auth = record(value)
    const principal = parsePrincipal(auth.principal)
    const state = record(auth.state)
    if (principal.device_id !== own.deviceId || expected && principal.network_id !== expected.networkId) throw new Error('电脑返回的身份和这台设备对不上。')
    if (expected?.kind === 'root' && (!same(state.genesis, expected.genesis) || expected.checkpoint !== undefined && !array(state.checkpoints).some(item => same(item, expected.checkpoint)))) throw new Error('电脑返回的设备记录和这里保存的对不上。')
    const view = verifiedView(state, principal.network_id, nodeNow(auth))
    if (principal.epoch !== view.epoch || !same(auth.view, view)) throw new Error('设备列表没通过校验。')
    const revoked = (id: string) => view.revoked.some(item => item.device_id === id)
    if (principal.source === 'local_session') {
      const node = expected?.kind === 'local' ? view.members.find(member => member.device_id === expected.nodeId) : undefined
      if (!node || node.kind !== 'node' || revoked(node.device_id)) throw new Error('本机连接和这台电脑对不上。')
      return { auth, principal, view }
    }
    const member = view.members.find(item => item.device_id === own.deviceId)
    if (!member || revoked(own.deviceId) || member.kind !== 'client' || principal.can_manage !== member.manage) throw new Error('设备权限没通过校验。')
    return { auth, principal, view }
  }
  /** Whether the device answered that it was removed, proven by the signed directory that removed it. */
  function verifyRemoval(value: unknown, nodeId: string, networkId: string): boolean {
    const auth = record(value)
    if (auth.node_revoked !== true) return false
    if (networkId && auth.network_id !== networkId) throw new Error('这台电脑属于另一组设备。请先确认连接的是自己的设备。')
    if (!verifiedView(auth.state, text(auth.network_id), nodeNow(auth)).revoked.some(item => item.device_id === nodeId)) throw new Error('设备说它已被移出，但记录没通过校验。')
    return true
  }
  /** Seals `root`, the current recovery key, with the material it belongs to. */
  function sealPackage(material: RecoveryMaterial, root: SecureIdentity, password: string): Uint8Array {
    const payload = { version: 1, network_id: material.networkId, root_secret_key: root.secretHex(), genesis: material.genesis, checkpoints: material.checkpoints, events: material.events }
    const cleartext = encoder.encode(JSON.stringify(payload))
    payload.root_secret_key = ''
    try {
      const envelope: unknown = JSON.parse(decoder.decode(secure.recoverySeal(cleartext, password)))
      return encoder.encode(JSON.stringify({ version: 1, network_id: material.networkId, envelope }))
    } finally { cleartext.fill(0) }
  }
  async function openPackage(file: RecoveryFile, password: string) {
    let networkId: string, envelope: Record<string, unknown>
    try {
      if (!file.size || file.size > 4 * 1024 * 1024) throw new Error()
      const bundle = record(JSON.parse(await file.text()))
      if (bundle.version !== 1) throw new Error()
      networkId = text(bundle.network_id); envelope = record(bundle.envelope)
    } catch { throw new Error(notRecoveryFile) }
    let cleartext: Uint8Array
    try { cleartext = secure.recoveryOpen(encoder.encode(JSON.stringify(envelope)), password) }
    catch { throw new Error('打不开恢复文件。请确认密码，并选下载时的那个文件。') }
    let payload: Record<string, unknown>
    try { payload = record(JSON.parse(decoder.decode(cleartext)), '恢复内容') } finally { cleartext.fill(0) }
    let root: SecureIdentity | undefined
    try {
      root = Identity.fromSecretHex(text(payload.root_secret_key))
      payload.root_secret_key = ''
      const material: RecoveryMaterial = { networkId, genesis: record(payload.genesis), checkpoints: array(payload.checkpoints), events: array(payload.events) }
      if (payload.version !== 1 || payload.network_id !== networkId) throw new Error(damagedRecoveryFile)
      verifiedView({ genesis: material.genesis, checkpoints: material.checkpoints, events: material.events }, networkId)
      // The file holds the key its last checkpoint hands recovery to, in effect yet or not.
      if ((material.checkpoints.length ? record(material.checkpoints.at(-1)).recovery_key : networkId) !== root.deviceId()) throw new Error(damagedRecoveryFile)
      const openedRoot = root
      let released = false
      return { material, root: openedRoot, networkId, release() { if (!released) { released = true; openedRoot.free() } } }
    } catch (error) { root?.free(); payload.root_secret_key = ''; throw error }
  }
  return {
    Identity, createGenesis, governancePayload, inviteSigningPayload, joinCommit, shortCode, ticketEncode, attestationPayload,
    verifyAddress, verifyState, verifiedView, verifyAuthentication, verifyRemoval, sealPackage, openPackage,
  }
}

/** Passwords people pick first; a trailing run of digits or symbols does not make one new. */
const COMMON = new Set(['password', 'passw0rd', 'p@ssw0rd', 'iloveyou', 'sunshine', 'princess', 'football', 'baseball', 'superman', 'welcome', 'letmein', 'monkey', 'dragon', 'master', 'admin', 'qwerty', 'abc123', 'woaini', 'woaini1314', 'vibeface', 'recovery'])
/** Keyboard and counting runs; a password inside one, either way round, is a pattern, not a secret. */
const RUNS = ['abcdefghijklmnopqrstuvwxyz', '01234567890', 'qwertyuiop', 'asdfghjkl', 'zxcvbnm', '1qaz2wsx3edc4rfv5tgb', 'qazwsxedcrfvtgbyhn']
export type PasswordProblem = 'short' | 'common' | 'simple'
export type PasswordStrength = { level: 'weak'; problem: PasswordProblem } | { level: 'fair' | 'strong' }
/**
 * How hard a new recovery password is to guess offline, by the rule docs/node.md#备份与恢复 states: at least 8
 * characters, not a common password or a keyboard, counting or repeated run, and under 12 characters it mixes letters,
 * digits and other characters. 16 or more, or 12 or more mixing two kinds, is strong.
 */
export function passwordStrength(password: string): PasswordStrength {
  const chars = [...password]
  if (chars.length < 8) return { level: 'weak', problem: 'short' }
  const lower = password.toLowerCase()
  if (COMMON.has(lower) || COMMON.has(lower.replace(/[^a-z]+$/, '')) || new Set(chars).size <= 3 || RUNS.some((run) => run.includes(lower) || [...run].reverse().join('').includes(lower))) return { level: 'weak', problem: 'common' }
  const kinds = [/[a-z]/i, /\d/, /[^a-z\d]/i].filter((kind) => kind.test(password)).length
  if (chars.length < 12 && kinds < 2) return { level: 'weak', problem: 'simple' }
  return { level: chars.length >= 16 || kinds >= 2 && chars.length >= 12 ? 'strong' : 'fair' }
}
const passwordProblems: Record<PasswordProblem, string> = { short: '恢复密码至少 8 个字符。', common: '这个恢复密码太常见了，换一个。', simple: '恢复密码少于 12 个字符时，要混用字母、数字或符号。' }
/** A new recovery password, or the reason it is refused. */
export function passphrase(password: string): string {
  if (encoder.encode(password).length > 1024) throw new Error('恢复密码太长了。')
  const strength = passwordStrength(password)
  if (strength.level === 'weak') throw new Error(passwordProblems[strength.problem])
  return password
}
/** The code a person matches between the settings page and a file name; never the key itself. */
export async function recoveryCode(recoveryKey: string): Promise<string> {
  const key = Uint8Array.from(recoveryKey.match(/../g) ?? [], (pair) => parseInt(pair, 16))
  return [...new Uint8Array(await crypto.subtle.digest('SHA-256', key)).slice(0, 4)].map((byte) => byte.toString(16).padStart(2, '0')).join('').toUpperCase()
}
/** The recovery file's name stem: the code people match against the file they kept. */
export const recoveryFileStem = (code: string) => `vibeface-recovery-${code}`
/** The name a new recovery file is saved under. */
export const recoveryFileName = (code: string) => `${recoveryFileStem(code)}-${new Date().toLocaleDateString('sv')}.json`
