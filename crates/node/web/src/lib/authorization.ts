import { signAuthorizedClientInvocation, signAuthorizedClientPrepare, prepareRemoteClientExecution, signRemoteClientExecution, signRemoteExecutionObservation } from '../../transport.js'
import { deriveAuthorizationId, revalidateClientPolicyAuthorization, verifyAccountDeviceDirectory, verifyProjectExecutionMetadata, verifyRemoteExecution, verifyRemoteExecutionResultHistory, verifyRemoteResolutionHistory } from '../../generated/vibeface_secure.js'
import type * as secure from '../../generated/vibeface_secure.js'
import { parseRemoteExecutionObservation, parseRemoteExecutionResolutionResult, parseRemoteExecutionSession, parseRemoteExecutionShare, parseTrustedRemoteExecution } from './remote-execution'
import type { RemoteExecutionObservation, RemoteExecutionResolutionResult, TrustedRemoteExecution } from './remote-execution'
import { array, record, same, strings } from './node'
import { parseIdentityScope } from './scope'
import type { AccountDeviceDirectory, AuthorizedRequest, ProjectResource, PublicIdentity } from './node'

import { mergeAuthorizationTrust, parseAuthorizationTrust, typedId, counter, stamp } from './authorization-trust'
import type { AuthorizationTrust } from './authorization-trust'
export { parseAuthorizationTrust, mergeAuthorizationTrust } from './authorization-trust'
export type { AuthorizationTrust } from './authorization-trust'

export type AuthorizationSelection = { resourceId: string; audience: { device: string; service: string }; grantLeaf?: string }

export function parseAuthorizationSelection(value: unknown): AuthorizationSelection {
  const selection = record(value), target = record(selection.audience)
  return { resourceId: typedId(selection.resourceId, 'res2_'), audience: { device: typedId(target.device, 'dev2_'), service: typedId(target.service, 'svc2_') }, ...(selection.grantLeaf === undefined ? {} : { grantLeaf: typedId(selection.grantLeaf, 'h2_') }) }
}

export function verifyScopedAuthentication(handle: secure.VerifiedClientAuthorization, own: PublicIdentity, trust: AuthorizationTrust, gatewayPublicKey: string, selection?: AuthorizationSelection) {
  let verified = handle
  let released = false
  try {
    verified.clientPolicyContextJson()
    const current = revalidateClientPolicyAuthorization(verified, JSON.stringify(trust.pins), Date.now())
    verified.free(); verified = current
    const scope = parseIdentityScope(JSON.parse(verified.scopeJson()))
    if (scope.account_id !== trust.accountId || scope.space_id !== trust.spaceId || scope.device_id !== deriveAuthorizationId('device', own.publicKey)) throw new Error('入口返回了其他账户、空间或设备范围。')
    const policy = record(JSON.parse(verified.policyJson()))
    const grantLeaf = typedId(policy.grant_leaf, 'h2_')
    if (selection?.grantLeaf !== undefined && grantLeaf !== selection.grantLeaf) throw new Error('入口返回的授权叶与已保存的可信项目选择不同。')
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
    let currentTrust: AuthorizationTrust = { ...trust, pins }
    const requireActive = () => { if (released) throw new Error('账户授权已释放，请重新连接。') }
    const adoptRemote = (handle: secure.VerifiedRemoteExecution, value: TrustedRemoteExecution) => {
      let disposed = false
      try {
        requireActive()
        const remote = parseTrustedRemoteExecution(value)
        const selection = parseRemoteExecutionShare({ ...remote.share, selection: JSON.parse(handle.selectionJson()) }).selection
        if (!same(selection, remote.share.selection)) throw new Error('远端政策与选定的执行授权不同。')
        const metadata = record(JSON.parse(handle.metadataJson()))
        const nextTrust = parseAuthorizationTrust({ ...remote.trust, pins: metadata.pins })
        const requireRemoteActive = () => { if (released || disposed) throw new Error('远端执行授权已释放，请重新核对。') }
        return {
          selection, trust: nextTrust, context: record(metadata.context),
          prepare(bodyJson: string, preparedContextJson: string, expectedAction: string, operationId: string) {
            requireRemoteActive()
            return prepareRemoteClientExecution(handle, verified, bodyJson, preparedContextJson, expectedAction, operationId, gatewayPublicKey)
          },
          sign(templateJson: string, bodyJson: string, expectedAction: string, operationId: string) {
            requireRemoteActive()
            return signRemoteClientExecution(handle, verified, templateJson, bodyJson, expectedAction, operationId, gatewayPublicKey)
          },
          validateReplay(bodyJson: string, invocationJson: string, expectedAction: string, operationId: string) {
            requireRemoteActive()
            handle.validateReplay(verified, bodyJson, invocationJson, expectedAction, operationId, gatewayPublicKey, Date.now())
          },
          session(receipt: Record<string, unknown>, binding: Record<string, unknown>) {
            requireRemoteActive()
            return parseRemoteExecutionSession(JSON.parse(handle.sessionJson(JSON.stringify(receipt), JSON.stringify(binding), Date.now())))
          },
          result(actual: Record<string, unknown>, original: AuthorizedRequest, binding: Record<string, unknown>) {
            requireRemoteActive()
            return parseRemoteExecutionObservation(JSON.parse(handle.resultJson(JSON.stringify(actual), JSON.stringify(original.body), JSON.stringify(original.invocation), JSON.stringify(binding), Date.now())))
          },
          observation(actual: Record<string, unknown>, binding: Record<string, unknown>) {
            requireRemoteActive()
            return parseRemoteExecutionObservation(JSON.parse(handle.observationJson(JSON.stringify(actual), JSON.stringify(binding), Date.now())))
          },
          observationRequest(original: Record<string, unknown>, authorizationId: string) {
            requireRemoteActive()
            return record(JSON.parse(signRemoteExecutionObservation(handle, JSON.stringify(original), authorizationId, gatewayPublicKey)))
          },
          resolution(actual: Record<string, unknown>, authorizationId: string, outcome: string, evidence: string) {
            requireRemoteActive()
            const intent = record(JSON.parse(handle.resolutionBodyJson(JSON.stringify(actual), authorizationId, outcome, evidence, Date.now())))
            return { body: record(intent.body), context: record(intent.context) }
          },
          finishResolution(actual: Record<string, unknown>, original: AuthorizedRequest) {
            requireRemoteActive()
            return parseRemoteExecutionResolutionResult(JSON.parse(handle.resolutionResultJson(JSON.stringify(actual), JSON.stringify(original.body), JSON.stringify(original.invocation), Date.now())))
          },
          release() { if (!disposed) { disposed = true; handle.free() } },
        }
      } catch (error) { handle.free(); throw error }
    }
    return {
      scope, audience, projects, actions, policy, enrollmentRights, authorityEpoch, grantLeaf,
      get trust() { return currentTrust },
      spaceGenesis: verified.spaceGenesisHash(), isTeam: verified.spaceKind() === 'team',
      clientPolicyContext() { requireActive(); return record(JSON.parse(verified.clientPolicyContextJson())) },
      revalidate(latestTrust: AuthorizationTrust) {
        requireActive()
        if (latestTrust.accountId !== scope.account_id || latestTrust.spaceId !== scope.space_id) throw new Error('最新政策水位属于其他账户或空间。')
        const nextTrust = mergeAuthorizationTrust(latestTrust, [currentTrust])
        const next = revalidateClientPolicyAuthorization(verified, JSON.stringify(nextTrust.pins), Date.now())
        const previous = verified
        verified = next; currentTrust = nextTrust; previous.free()
        return currentTrust
      },
      withNative<T>(work: (authorization: secure.VerifiedClientAuthorization) => T): T { requireActive(); return work(verified) },
      accountRecoveryPolicy() {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        const recovery = record(JSON.parse(verified.accountRecoveryPolicyJson()))
        return { threshold: counter(recovery.threshold), keys: strings(recovery.keys) }
      },
      enrollmentMaterial() {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return { pins: currentTrust.pins, grantLeaf, expiresAt: policy.account_enrollment_expires_at_ms === null ? null : counter(policy.account_enrollment_expires_at_ms) }
      },
      prepare(bodyJson: string, contextJson: string, action: string, operationId: string) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return signAuthorizedClientPrepare(verified, bodyJson, contextJson, action, operationId, gatewayPublicKey)
      },
      sign(templateJson: string, bodyJson: string, action: string, operationId: string) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return signAuthorizedClientInvocation(verified, templateJson, bodyJson, action, operationId, policy.audience_public_key === gatewayPublicKey ? undefined : gatewayPublicKey)
      },
      adoptRemote,
      remote(value: TrustedRemoteExecution, policy: secure.VerifiedRemoteExecutionPolicy): ReturnType<typeof adoptRemote> {
        requireActive()
        const remote = parseTrustedRemoteExecution(value)
        const currentPolicy = policy.revalidatePins(JSON.stringify(remote.trust.pins), Date.now())
        try { return adoptRemote(verifyRemoteExecution(verified, currentPolicy, Date.now()), remote) }
        finally { currentPolicy.free() }
      },
      projectExecutionMetadata(source: Record<string, unknown>, binding: Record<string, unknown>) {
        if (released) throw new Error('账户授权已释放，请重新连接。')
        return record(JSON.parse(verifyProjectExecutionMetadata(verified, JSON.stringify(source), JSON.stringify(binding))))
      },
      remoteExecutionResultHistory(value: TrustedRemoteExecution, policy: secure.VerifiedRemoteExecutionHistoryPolicy, actual: Record<string, unknown>, original: AuthorizedRequest, binding: Record<string, unknown>): RemoteExecutionObservation {
        requireActive()
        const remote = parseTrustedRemoteExecution(value)
        const currentPolicy = policy.revalidatePins(JSON.stringify(remote.trust.pins), Date.now())
        try {
          const result = record(JSON.parse(verifyRemoteExecutionResultHistory(verified, currentPolicy, JSON.stringify(actual), JSON.stringify(original.body), JSON.stringify(original.invocation), JSON.stringify(binding), Date.now())))
          return parseRemoteExecutionObservation(result.actual)
        } finally { currentPolicy.free() }
      },
      remoteResolutionHistory(value: TrustedRemoteExecution, policy: secure.VerifiedRemoteExecutionHistoryPolicy, actual: Record<string, unknown>, original: AuthorizedRequest): RemoteExecutionResolutionResult {
        requireActive()
        const remote = parseTrustedRemoteExecution(value)
        const currentPolicy = policy.revalidatePins(JSON.stringify(remote.trust.pins), Date.now())
        try {
          return parseRemoteExecutionResolutionResult(JSON.parse(verifyRemoteResolutionHistory(verified, currentPolicy, JSON.stringify(actual), JSON.stringify(original.body), JSON.stringify(original.invocation), Date.now())))
        } finally { currentPolicy.free() }
      },
      directory(): AccountDeviceDirectory {
        requireActive()
        const accountHead = heads.find(head => head.domain.kind === 'account' && head.domain.id === scope.account_id)
        if (!accountHead) throw new Error('已确认快照缺少账户政策头。')
        const devices: AccountDeviceDirectory['devices'] = []
        const cursors = new Set<string>()
        let cursor: string | null = null
        for (;;) {
          const page = record(JSON.parse(verifyAccountDeviceDirectory(verified, JSON.stringify(cursor), Date.now())))
          if (page.account !== scope.account_id || !same(stamp(page.head), accountHead)) throw new Error('设备目录切换了账户或已确认政策头。')
          const entries = array(page.devices)
          if (entries.length > 1) throw new Error('设备目录页面超过单条记录限制。')
          for (const value of entries) {
            const entry = record(value), certificate = record(record(entry.certificate).payload), owner = record(certificate.owner), subject = record(certificate.subject)
            const publicKey = typedId(certificate.public_key, '')
            if (certificate.version !== 2 || owner.kind !== 'account' || owner.id !== scope.account_id
              || certificate.device !== deriveAuthorizationId('device', publicKey) || subject.kind !== 'account' && subject.kind !== 'service'
              || entry.status !== 'active' && entry.status !== 'expired' && entry.status !== 'pending') throw new Error('设备目录记录的所属账户、设备或状态无效。')
            devices.push({ device_id: certificate.device, public_key: publicKey,
              subject: { kind: subject.kind, id: typedId(subject.id, subject.kind === 'account' ? 'acc2_' : 'svc2_') }, uses: strings(certificate.uses),
              not_before_ms: counter(certificate.not_before_ms), expires_at_ms: counter(certificate.expires_at_ms), status: entry.status })
          }
          if (page.next_cursor === null) return { account_id: scope.account_id, devices }
          const next = typedId(page.next_cursor, 'h2_')
          if (!entries.length || cursors.has(next)) throw new Error('设备目录分页没有继续推进。')
          cursors.add(next); cursor = next
        }
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
