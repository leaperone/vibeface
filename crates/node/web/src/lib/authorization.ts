import { signAuthorizedClientInvocation, signAuthorizedClientPrepare } from '../../transport.js'
import { verifyAccountDeviceDirectory, verifyClientAuthorization } from '../../generated/vibeface_secure.js'
import { array, record, strings, text } from './node'
import { parseIdentityScope, sameScope } from './scope'
import type { AccountDeviceDirectory, ProjectResource, PublicIdentity } from './node'

type PolicyDomain = { kind: 'account' | 'space'; id: string }
type PolicyStamp = { domain: PolicyDomain; authority_epoch: number; sequence: number; head: string }
type PolicyPin = { domain: PolicyDomain; genesis_root: string; minimum_head: PolicyStamp | null }
export type AuthorizationTrust = { kind: 'policy'; accountId: string; spaceId: string; pins: PolicyPin[] }
export type AuthorizationSelection = { resourceId: string; audience: { device: string; service: string } }

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
export function parseAuthorizationSelection(value: unknown): AuthorizationSelection {
  const selection = record(value), target = record(selection.audience)
  return { resourceId: typedId(selection.resourceId, 'res2_'), audience: { device: typedId(target.device, 'dev2_'), service: typedId(target.service, 'svc2_') } }
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

function parseAccountDirectory(value: unknown): AccountDeviceDirectory {
  const input = record(value)
  return {
    account_id: typedId(input.account, 'acc2_'),
    devices: array(input.devices).map(value => {
      const entry = record(value), device = record(record(entry.certificate).payload), subject = record(device.subject)
      if (subject.kind !== 'account' && subject.kind !== 'service') throw new Error('已验证的账户设备主体格式无效。')
      if (entry.status !== 'active' && entry.status !== 'expired' && entry.status !== 'pending') throw new Error('已验证的账户设备状态无效。')
      return { device_id: typedId(device.device, 'dev2_'), public_key: typedId(device.public_key, ''),
        subject: { kind: subject.kind, id: typedId(subject.id, subject.kind === 'account' ? 'acc2_' : 'svc2_') },
        uses: strings(device.uses), not_before_ms: counter(device.not_before_ms), expires_at_ms: counter(device.expires_at_ms), status: entry.status,
      }
    }),
  }
}

export function verifyScopedAuthentication(value: unknown, own: PublicIdentity, trust: AuthorizationTrust, gatewayPublicKey: string, selection?: AuthorizationSelection) {
  const auth = record(value)
  const verified = verifyClientAuthorization(JSON.stringify(record(auth.authorization)), JSON.stringify(trust.pins), trust.accountId, trust.spaceId, own.publicKey, Date.now())
  let released = false
  try {
    const scope = parseIdentityScope(JSON.parse(verified.scopeJson()))
    if (!sameScope(scope, parseIdentityScope(auth.scope))) throw new Error('入口返回了其他账户、空间或设备范围。')
    const policy = record(JSON.parse(verified.policyJson()))
    if (!selection && policy.audience_public_key !== gatewayPublicKey) throw new Error('授权资源与当前加密连接的目标设备不一致。')
    const target = record(policy.audience)
    const audience = { device: typedId(target.device, 'dev2_'), service: typedId(target.service, 'svc2_') }
    if (selection && (audience.device !== selection.audience.device || audience.service !== selection.audience.service)) throw new Error('授权目标与已保存的可信项目选择不一致。')
    const projects: ProjectResource[] = array(policy.resources).map(value => {
      const resource = record(record(value).payload)
      if (resource.version !== 2 || resource.kind !== 'project' || resource.space !== scope.space_id || resource.host !== audience.service) throw new Error('已验证的项目记录与当前空间或目标不一致。')
      return { id: typedId(resource.resource, 'res2_'), host: audience.service }
    })
    if (new Set(projects.map(project => project.id)).size !== projects.length) throw new Error('项目授权包含重复记录。')
    if (selection && !projects.some(project => project.id === selection.resourceId)) throw new Error('授权没有覆盖已保存的目标项目。')
    const heads = array(policy.heads).map(stamp)
    const authorityEpoch = heads.find(head => head.domain.kind === 'space' && head.domain.id === scope.space_id)?.authority_epoch
    if (authorityEpoch === undefined) throw new Error('授权缺少当前空间政策。')
    const actions = strings(policy.actions)
    const enrollmentRights = strings(policy.account_enrollment_rights)
    const pins = trust.pins.map(pin => ({ ...pin, minimum_head: heads.find(head => head.domain.kind === pin.domain.kind && head.domain.id === pin.domain.id) ?? pin.minimum_head }))
    return {
      scope, audience, projects, actions, policy, enrollmentRights, authorityEpoch, trust: { ...trust, pins },
      enrollmentMaterial() {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return { pins, bundle: record(auth.authorization), expiresAt: policy.account_enrollment_expires_at_ms === null ? null : counter(policy.account_enrollment_expires_at_ms) }
      },
      prepare(bodyJson: string, contextJson: string, action: string, operationId: string) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return signAuthorizedClientPrepare(verified, bodyJson, contextJson, action, operationId, gatewayPublicKey)
      },
      sign(templateJson: string, bodyJson: string, action: string, operationId: string) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return signAuthorizedClientInvocation(verified, templateJson, bodyJson, action, operationId, policy.audience_public_key === gatewayPublicKey ? undefined : gatewayPublicKey)
      },
      directory(value: unknown) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        const response = record(value)
        return parseAccountDirectory(JSON.parse(verifyAccountDeviceDirectory(verified, JSON.stringify(record(response.authorization)), JSON.stringify(pins), JSON.stringify(array(response.devices)), Date.now())))
      },
      release() { if (!released) { released = true; verified.free() } },
    }
  } catch (error) { verified.free(); throw error }
}

export function subscriptionRenewalDelay(expiresAt: number, now: number) {
  const remaining = expiresAt - now
  if (remaining <= 0) throw new Error('订阅授权已过期，请重新连接。')
  return Math.min(60000, Math.floor(remaining / 2))
}
