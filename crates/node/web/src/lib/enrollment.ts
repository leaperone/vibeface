import { createClientEnrollmentIntent, validTarget } from '../../transport.js'
import { verifyDeviceEnrollmentOffer } from '../../generated/vibeface_secure.js'
import { parseAuthorizationTrust } from './authorization'
import { array, record, same, strings, text } from './node'
import type { EnrollmentReference } from './enrollment-reference'

export type EnrollmentStatus = 'requested' | 'approved' | 'possessed' | 'completed'
export type EnrollmentRequest = {
  requestId: string
  nonce: string
  newDevicePublicKey: string
  status: EnrollmentStatus
  unsignedIntent: Record<string, unknown>
  intent: Record<string, unknown> | null
  certificate: Record<string, unknown> | null
  completion: Record<string, unknown> | null
}
export type EnrollmentCall = (method: string, params: Record<string, unknown>) => Promise<unknown>

export function parseEnrollmentRequest(value: unknown): EnrollmentRequest {
  const input = record(value, '设备接入申请'), requestId = text(input.request_id)
  if (!/^h2_[a-f0-9]{64}$/.test(requestId)) throw new Error('设备接入申请编号无效。')
  const status = input.status
  if (status !== 'requested' && status !== 'approved' && status !== 'possessed' && status !== 'completed') throw new Error('设备接入申请状态无效。')
  return {
    requestId, nonce: validTarget(input.nonce), newDevicePublicKey: validTarget(input.new_device_public_key), status,
    unsignedIntent: record(input.unsigned_intent), intent: input.intent === undefined || input.intent === null ? null : record(input.intent),
    certificate: input.certificate === undefined || input.certificate === null ? null : record(input.certificate), completion: input.completion === undefined || input.completion === null ? null : record(input.completion),
  }
}

export function checkEnrollmentOffer(offerJson: string, reference: EnrollmentReference) {
  const verified = verifyDeviceEnrollmentOffer(offerJson, Date.now())
  try {
    if (verified.offerHash() !== reference.offer_hash || verified.accountId() !== reference.account_id || verified.expiresAtMs() !== reference.expires_at_ms
      || !same(JSON.parse(verified.entryJson()), { endpoint: reference.endpoint, node_public_key: reference.node_public_key })) throw new Error('取得的设备邀请与扫码时固定的账户、节点或摘要不同。')
    const pins = array(JSON.parse(verified.pinsJson()))
    const space = pins.map(value => record(record(value).domain)).find(domain => domain.kind === 'space')
    const trust = parseAuthorizationTrust({ kind: 'policy', accountId: reference.account_id, spaceId: text(space?.id), pins })
    return { trust, rights: strings(JSON.parse(verified.requestedRightsJson())), unsignedIntent: record(JSON.parse(createClientEnrollmentIntent(verified))) }
  } finally { verified.free() }
}

export async function requestDeviceEnrollment(reference: EnrollmentReference, ownPublicKey: string, call: EnrollmentCall, signal: AbortSignal) {
  signal.throwIfAborted()
  const response = record(await call('enrollment_offer_get', { account_id: reference.account_id, offer_hash: reference.offer_hash }))
  signal.throwIfAborted()
  const offerJson = JSON.stringify(record(response.offer)), checked = checkEnrollmentOffer(offerJson, reference)
  if (checked.unsignedIntent.new_device_public_key !== ownPublicKey) throw new Error('设备接入意图未绑定当前浏览器的独立公钥。')
  const pending = record(await call('enrollment_request', { account_id: reference.account_id, offer_hash: reference.offer_hash, new_device_public_key: ownPublicKey, intent: checked.unsignedIntent }))
  signal.throwIfAborted()
  const requestId = text(pending.request_id), nonce = validTarget(pending.nonce)
  if (!/^h2_[a-f0-9]{64}$/.test(requestId) || pending.status !== 'requested') throw new Error('节点未确认原始设备接入申请。')
  return { ...checked, reference, offerJson, requestId, nonce }
}

export function verifyEnrollmentStatus(value: unknown, original: Awaited<ReturnType<typeof requestDeviceEnrollment>>) {
  const request = parseEnrollmentRequest(value)
  if (request.requestId !== original.requestId || request.nonce !== original.nonce || request.newDevicePublicKey !== original.unsignedIntent.new_device_public_key
    || !same(request.unsignedIntent, original.unsignedIntent)
    || request.intent && !same(record(request.intent.payload), original.unsignedIntent)) throw new Error('节点返回的接入状态与原设备密钥或原接入意图不同。')
  return request
}
