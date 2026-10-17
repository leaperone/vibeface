import { signAuthorizedClientInvocation } from '../../transport.js'
import { verifyClientAuthorization } from '../../generated/vibeface_secure.js'
import { array, record, strings, text } from './node'
import { parseIdentityScope, sameScope } from './scope'
import type { PublicIdentity } from './node'

type PolicyDomain = { kind: 'account' | 'space'; id: string }
type PolicyStamp = { domain: PolicyDomain; authority_epoch: number; sequence: number; head: string }
type PolicyPin = { domain: PolicyDomain; genesis_root: string; minimum_head: PolicyStamp | null }
export type AuthorizationTrust = { kind: 'policy'; accountId: string; spaceId: string; pins: PolicyPin[] }

function typedId(value: unknown, prefix: string) {
  const id = text(value)
  if (!new RegExp(`^${prefix}[a-f0-9]{64}$`).test(id)) throw new Error('账户授权标识格式无效。')
  return id
}
function domain(value: unknown): PolicyDomain {
  const input = record(value)
  if (input.kind !== 'account' && input.kind !== 'space') throw new Error('政策范围格式无效。')
  return { kind: input.kind, id: typedId(input.id, input.kind === 'account' ? 'acc2_' : 'spc2_') }
}
function counter(value: unknown) {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new Error('政策版本格式无效。')
  return value
}
function stamp(value: unknown): PolicyStamp {
  const input = record(value)
  return { domain: domain(input.domain), authority_epoch: counter(input.authority_epoch), sequence: counter(input.sequence), head: typedId(input.head, 'h2_') }
}
export function parseAuthorizationTrust(value: unknown): AuthorizationTrust {
  const input = record(value, '已保存的账户信任')
  if (input.kind !== 'policy') throw new Error('需要新格式账户连接卡或当前格式的设备备份。')
  const pins = array(input.pins).map(value => {
    const pin = record(value)
    return { domain: domain(pin.domain), genesis_root: typedId(pin.genesis_root, ''), minimum_head: pin.minimum_head === null ? null : stamp(pin.minimum_head) }
  })
  if (!pins.length || pins.length > 10) throw new Error('账户根信任数量无效。')
  return { kind: 'policy', accountId: typedId(input.accountId, 'acc2_'), spaceId: typedId(input.spaceId, 'spc2_'), pins }
}

function newerHead(left: PolicyStamp | null, right: PolicyStamp | null): PolicyStamp | null {
  if (!left) return right
  if (!right) return left
  if (left.authority_epoch === right.authority_epoch && left.sequence === right.sequence) {
    if (left.head !== right.head) throw new Error('政策在相同版本出现不同签名历史，已暂停连接。')
    return left
  }
  return left.authority_epoch > right.authority_epoch || left.authority_epoch === right.authority_epoch && left.sequence > right.sequence ? left : right
}

export function mergeAuthorizationTrust(candidate: AuthorizationTrust, known: AuthorizationTrust[]): AuthorizationTrust {
  if (known.some(trust => trust.accountId !== candidate.accountId)) throw new Error('此设备配置已属于另一个账户。请为新账户创建独立设备配置。')
  const pins = candidate.pins.map(pin => {
    let minimum = pin.minimum_head
    for (const trust of known) {
      const saved = trust.pins.find(other => other.domain.kind === pin.domain.kind && other.domain.id === pin.domain.id)
      if (!saved) continue
      if (saved.genesis_root !== pin.genesis_root) throw new Error('账户或空间根信任与已保存配置不一致。')
      minimum = newerHead(minimum, saved.minimum_head)
    }
    return { ...pin, minimum_head: minimum }
  })
  return { ...candidate, pins }
}

export function verifyScopedAuthentication(value: unknown, own: PublicIdentity, trust: AuthorizationTrust, targetPublicKey: string) {
  const auth = record(value)
  const verified = verifyClientAuthorization(JSON.stringify(record(auth.authorization)), JSON.stringify(trust.pins), trust.accountId, trust.spaceId, own.publicKey, Date.now())
  let released = false
  try {
    const scope = parseIdentityScope(JSON.parse(verified.scopeJson()))
    if (!sameScope(scope, parseIdentityScope(auth.scope))) throw new Error('入口返回了其他账户、空间或设备范围。')
    const policy = record(JSON.parse(verified.policyJson()))
    if (policy.audience_public_key !== targetPublicKey) throw new Error('授权资源与当前加密连接的目标设备不一致。')
    const target = record(policy.audience)
    const audience = { device: typedId(target.device, 'dev2_'), service: typedId(target.service, 'svc2_') }
    const heads = array(policy.heads).map(stamp)
    const authorityEpoch = heads.find(head => head.domain.kind === 'space' && head.domain.id === scope.space_id)?.authority_epoch
    if (authorityEpoch === undefined) throw new Error('授权缺少当前空间政策。')
    const actions = strings(policy.actions)
    const enrollmentRights = strings(policy.account_enrollment_rights)
    const pins = trust.pins.map(pin => ({ ...pin, minimum_head: heads.find(head => head.domain.kind === pin.domain.kind && head.domain.id === pin.domain.id) ?? pin.minimum_head }))
    return {
      scope, audience, actions, policy, enrollmentRights, authorityEpoch, trust: { ...trust, pins },
      sign(templateJson: string, bodyJson: string, action: string, operationId: string) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return signAuthorizedClientInvocation(verified, templateJson, bodyJson, action, operationId)
      },
      release() { if (!released) { released = true; verified.free() } },
    }
  } catch (error) { verified.free(); throw error }
}
