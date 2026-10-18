import { deriveAuthorizationBodyHash, deriveAuthorizationId, prepareAuthorizationSigning, verify } from '../../generated/vibeface_secure.js'
import { record, same, text } from './node'
import type { AuthorizedRequest, ReadyConnection, RpcFacts } from './node'
import { prepareContext, verifyPreparedBody } from './rpc-prepare'

export function validateRpcReplay(input: {
  proof: AuthorizedRequest
  body: Record<string, unknown>
  facts?: RpcFacts
  scope: ReadyConnection['scope']
  projectId: string
  audience: ReadyConnection['audience']
  ownPublicKey: string
  gatewayPublicKey: string
  audiencePublicKey: string
  action: string
  operationId: string
  remoteReplay?: (bodyJson: string, invocationJson: string, action: string, operationId: string) => void
  projectMetadata?: Record<string, unknown>
}): AuthorizedRequest {
  const { proof, body, facts, scope, projectId, audience, ownPublicKey, gatewayPublicKey, audiencePublicKey, action, operationId } = input
  verifyPreparedBody(body, proof.body, facts)
  if (facts?.remote) {
    if (!input.remoteReplay) throw new Error('远端原请求缺少独立授权重放验证，已保留待核对记录。')
    input.remoteReplay(JSON.stringify(proof.body), JSON.stringify(proof.invocation), action, operationId)
    return proof
  }
  const invocation = record(proof.invocation), payload = record(invocation.payload)
  const actor = { kind: 'account', id: scope.account_id }
  const presenter = gatewayPublicKey === audiencePublicKey ? scope.device_id : deriveAuthorizationId('device', gatewayPublicKey)
  const expectedContext = prepareContext(proof.body, audience.device, projectId, facts, input.projectMetadata, audiencePublicKey).context
  if (Object.keys(invocation).length !== 3 || invocation.issuer !== ownPublicKey
    || !same(payload.actor, actor) || !same(payload.initiator, actor) || payload.device !== scope.device_id
    || !same(payload.resource, { space: scope.space_id, resource: projectId })
    || !same(payload.audience, audience) || payload.presenter !== presenter
    || !same(payload.context, expectedContext) || payload.operation_id !== operationId || payload.action !== action
    || payload.body_hash !== deriveAuthorizationBodyHash(JSON.stringify(proof.body))) {
    throw new Error('已保存的签名请求与当前账户、项目、入口或原操作不同，已保留记录，请核对。')
  }
  const signing = record(JSON.parse(prepareAuthorizationSigning('invocation', JSON.stringify(payload), ownPublicKey)))
  verify(ownPublicKey, text(signing.domain), new TextEncoder().encode(text(signing.signing_bytes)), text(invocation.signature))
  return proof
}
