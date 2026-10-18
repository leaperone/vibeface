import { signAuthorizedClientInvocation, signAuthorizedClientPrepare } from '../../transport.js'
import type { DeviceChannel } from '../../transport.js'
import { deriveAuthorizationId, revalidateClientPolicyAuthorization } from '../../generated/vibeface_secure.js'
import type { VerifiedClientAuthorization } from '../../generated/vibeface_secure.js'
import type { AuthorizationSelection, AuthorizationTrust } from './authorization'
import { parseAuthorizationTrust } from './authorization'
import { readClientPolicyAuthorization } from './client-policy'
import { array, record, same, strings, text } from './node'
import type { AuthorizedReply, AuthorizedRequest, PublicIdentity } from './node'
import { parseIdentityScope } from './scope'
import { prepareRpc } from './rpc-action'
import { prepareContext, verifyPreparedBody } from './rpc-prepare'
import { validateRpcReplay } from './rpc-replay'

export async function callExecutionResource(input: {
  channel: DeviceChannel
  nodePublicKey: string
  own: PublicIdentity
  trust: AuthorizationTrust
  selection: AuthorizationSelection
  body: Record<string, unknown>
  check(): void
  withTrust<T>(work: () => T): Promise<T>
  latestTrust(): AuthorizationTrust
  persistTrust(trust: AuthorizationTrust): Promise<void>
  beforeSend?(proof: AuthorizedRequest): void
  replay?: AuthorizedRequest
}): Promise<AuthorizedReply> {
  input.check()
  if (input.channel.publicKey !== input.nodePublicKey || !input.selection.grantLeaf
    || !['seal_remote_execution', 'remote_execution_seal_status'].includes(text(input.body.method))) throw new Error('封存只允许原执行端的独立精确授权。')
  const { method: methodValue, space_id: spaceValue, authorization_id: authorizationValue, ...params } = input.body
  const prepared = prepareRpc({ method: text(methodValue), spaceId: text(spaceValue), authorizationId: text(authorizationValue), params })
  if (!same(prepared.body, input.body) || input.body.resource_id !== input.selection.resourceId || input.body.space_id !== input.trust.spaceId) throw new Error('封存请求与独立授权的执行资源不同。')
  const audience = { device: deriveAuthorizationId('device', input.nodePublicKey), service: deriveAuthorizationId('service', input.nodePublicKey) }
  if (!same(audience, input.selection.audience)) throw new Error('封存授权目标与原执行端公钥不同。')
  const rawContext = prepareContext(prepared.body, audience.device, input.selection.resourceId, undefined, undefined, input.nodePublicKey)
  const call = async (method: string, params: Record<string, unknown>, invocation?: unknown) => {
    input.check()
    const result = await input.channel.request(method, params, invocation)
    input.check()
    return result
  }
  let authorization: VerifiedClientAuthorization | null = null
  try {
    authorization = await readClientPolicyAuthorization({ call: (method, params) => call('identity', { method, params }), trust: input.trust,
      gatewayPublicKey: input.nodePublicKey, selection: input.selection, check: input.check })
    const current = <T,>(work: (authorization: VerifiedClientAuthorization) => T) => input.withTrust(() => {
      input.check()
      if (!authorization) throw new Error('封存授权已释放，请核对原请求。')
      const trust = input.latestTrust()
      if (trust.accountId !== input.trust.accountId || trust.spaceId !== input.trust.spaceId) throw new Error('最新独立信任改变了原执行授权范围。')
      const next = revalidateClientPolicyAuthorization(authorization, JSON.stringify(trust.pins), Date.now())
      authorization.free(); authorization = next
      const scope = parseIdentityScope(JSON.parse(next.scopeJson())), policy = record(JSON.parse(next.policyJson()))
      if (scope.account_id !== input.trust.accountId || scope.space_id !== input.trust.spaceId || scope.device_id !== deriveAuthorizationId('device', input.own.publicKey)
        || policy.grant_leaf !== input.selection.grantLeaf || policy.audience_public_key !== input.nodePublicKey || !same(policy.audience, audience)
        || !strings(policy.actions).includes(prepared.action) || !array(policy.resources).some(value => {
          const resource = record(record(value).payload)
          return resource.space === scope.space_id && resource.resource === input.selection.resourceId && resource.kind === 'execution_service' && resource.host === audience.service
        })) throw new Error('当前授权未覆盖原执行端的精确封存或状态读取。')
      return work(next)
    })
    const expected = await current(handle => ({ context: record(JSON.parse(handle.clientPolicyContextJson())), scope: JSON.parse(handle.scopeJson()), policy: record(JSON.parse(handle.policyJson())) }))
    const heads = array(expected.policy.heads).map(value => record(value))
    await input.persistTrust(parseAuthorizationTrust({ ...input.trust, pins: input.trust.pins.map(pin => ({ ...pin, minimum_head: heads.find(head => same(head.domain, pin.domain)) ?? pin.minimum_head })) }))
    const authenticated = record(await call('authenticate', expected.context))
    if (!same(authenticated.scope, expected.scope) || !same(authenticated.policy, expected.policy) || !same(authenticated.resources, expected.policy.resources)) throw new Error('原执行端会话与已确认的有限授权不同。')
    let proof: AuthorizedRequest
    if (input.replay) {
      proof = await current(handle => validateRpcReplay({ proof: input.replay!, body: prepared.body,
        scope: parseIdentityScope(JSON.parse(handle.scopeJson())), projectId: input.selection.resourceId, audience,
        ownPublicKey: input.own.publicKey, gatewayPublicKey: input.nodePublicKey, audiencePublicKey: input.nodePublicKey,
        action: prepared.action, operationId: prepared.authorizationId }))
    } else {
      const intent = await current(handle => record(JSON.parse(signAuthorizedClientPrepare(handle, JSON.stringify(prepared.body), JSON.stringify(rawContext), prepared.action, prepared.authorizationId, input.nodePublicKey))))
      const response = record(await call('prepare_invocation', { body: prepared.body, intent }))
      const body = record(response.body), template = record(response.invocation)
      verifyPreparedBody(prepared.body, body)
      if (!same(template.context, rawContext.context)) throw new Error('原执行端改变了封存的任务或会话上下文。')
      const invocation = await current(handle => JSON.parse(signAuthorizedClientInvocation(handle, JSON.stringify(template), JSON.stringify(body), prepared.action, prepared.authorizationId, input.nodePublicKey)))
      proof = { body, invocation }
    }
    input.check()
    input.beforeSend?.(proof)
    return { ...proof, result: await call('rpc', proof.body, proof.invocation) }
  } finally { authorization?.free() }
}
