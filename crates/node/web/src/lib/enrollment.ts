import { beginClientEnrollmentOfferImport, buildClientEnrollmentPolicyReadRequest, createClientEnrollmentIntent, validTarget } from '../../transport.js'
import { verifyDeviceEnrollmentOffer } from '../../generated/vibeface_secure.js'
import type { EnrollmentOfferDescriptor, VerifiedEnrollmentOffer } from '../../generated/vibeface_secure.js'
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

function inspectEnrollmentOffer(descriptor: EnrollmentOfferDescriptor, reference: EnrollmentReference) {
  if (descriptor.offerHash() !== reference.offer_hash || descriptor.accountId() !== reference.account_id || descriptor.expiresAtMs() !== reference.expires_at_ms
    || !same(JSON.parse(descriptor.entryJson()), { endpoint: reference.endpoint, node_public_key: reference.node_public_key })) throw new Error('取得的设备邀请与扫码时固定的账户、节点或摘要不同。')
  const pins = array(JSON.parse(descriptor.pinsJson()))
  const space = pins.map(value => record(record(value).domain)).find(domain => domain.kind === 'space')
  const trust = parseAuthorizationTrust({ kind: 'policy', accountId: reference.account_id, spaceId: text(space?.id), pins })
  return { trust, rights: strings(JSON.parse(descriptor.requestedRightsJson())) }
}

export function checkEnrollmentOffer(offerJson: string, reference: EnrollmentReference) {
  const descriptor = verifyDeviceEnrollmentOffer(offerJson, Date.now())
  try { return inspectEnrollmentOffer(descriptor, reference) }
  finally { descriptor.free() }
}

export async function readVerifiedEnrollmentOffer(offerJson: string, reference: EnrollmentReference, ownPublicKey: string, call: EnrollmentCall, signal: AbortSignal): Promise<VerifiedEnrollmentOffer> {
  const read = async (method: string, params: Record<string, unknown>) => {
    signal.throwIfAborted()
    const response = await call(method, params)
    signal.throwIfAborted()
    return record(response)
  }
  signal.throwIfAborted()
  const descriptor = verifyDeviceEnrollmentOffer(offerJson, Date.now())
  let importer: ReturnType<typeof beginClientEnrollmentOfferImport> | null = null
  let offer: VerifiedEnrollmentOffer | null = null
  try {
    inspectEnrollmentOffer(descriptor, reference)
    const requestJson = buildClientEnrollmentPolicyReadRequest(descriptor, Math.min(reference.expires_at_ms, Date.now() + 120_000))
    const request = record(JSON.parse(requestJson)), reader = record(request.payload)
    if (request.issuer !== ownPublicKey || reader.reader_public_key !== ownPublicKey || reader.target_public_key !== ownPublicKey) throw new Error('设备邀请策略读取请求未绑定当前浏览器的独立公钥。')
    const begun = await read('enrollment_policy_read_begin', {
      credential: { purpose: 'browser_device', credential: JSON.parse(offerJson) }, request,
    })
    importer = beginClientEnrollmentOfferImport(descriptor, requestJson, JSON.stringify(begun.snapshot), reference.node_public_key)
    for (;;) {
      const response = await read('enrollment_policy_read_page', record(JSON.parse(importer.pageRequestJson())))
      const page = record(response.page)
      importer.appendPage(JSON.stringify(page), Date.now())
      if (record(page.payload).complete === true) break
    }
    const witnessParams = record(JSON.parse(importer.pageRequestJson()))
    const witnessed = await read('enrollment_policy_read_witness', { ...witnessParams, cursor: null })
    offer = importer.finish(JSON.stringify(record(witnessed.witness)), Date.now())
    signal.throwIfAborted()
    const verifiedOffer = offer
    offer = null
    return verifiedOffer
  } finally { offer?.free(); importer?.free(); descriptor.free() }
}

export async function requestDeviceEnrollment(reference: EnrollmentReference, ownPublicKey: string, call: EnrollmentCall, signal: AbortSignal) {
  signal.throwIfAborted()
  const response = record(await call('enrollment_offer_get', { account_id: reference.account_id, offer_hash: reference.offer_hash }))
  signal.throwIfAborted()
  const offerJson = JSON.stringify(record(response.offer))
  let offer: VerifiedEnrollmentOffer | null = await readVerifiedEnrollmentOffer(offerJson, reference, ownPublicKey, call, signal)
  try {
    const checked = inspectEnrollmentOffer(offer, reference)
    const unsignedIntent = record(JSON.parse(createClientEnrollmentIntent(offer)))
    if (unsignedIntent.new_device_public_key !== ownPublicKey) throw new Error('设备接入意图未绑定当前浏览器的独立公钥。')
    signal.throwIfAborted()
    const pending = record(await call('enrollment_request', { account_id: reference.account_id, offer_hash: reference.offer_hash, new_device_public_key: ownPublicKey, intent: unsignedIntent }))
    signal.throwIfAborted()
    const requestId = text(pending.request_id), nonce = validTarget(pending.nonce)
    if (!/^h2_[a-f0-9]{64}$/.test(requestId) || pending.status !== 'requested') throw new Error('节点未确认原始设备接入申请。')
    const verifiedOffer = offer
    offer = null
    let released = false
    return {
      ...checked, reference, offerJson, requestId, nonce, unsignedIntent, offer: verifiedOffer,
      release() { if (!released) { released = true; verifiedOffer.free() } },
    }
  } finally { offer?.free() }
}

export function verifyEnrollmentStatus(value: unknown, original: Awaited<ReturnType<typeof requestDeviceEnrollment>>) {
  const request = parseEnrollmentRequest(value)
  if (request.requestId !== original.requestId || request.nonce !== original.nonce || request.newDevicePublicKey !== original.unsignedIntent.new_device_public_key
    || !same(request.unsignedIntent, original.unsignedIntent)
    || request.intent && !same(record(request.intent.payload), original.unsignedIntent)) throw new Error('节点返回的接入状态与原设备密钥或原接入意图不同。')
  return request
}
