import { beginClientEnrollmentOfferImport, buildClientEnrollmentPolicyReadRequest, createClientEnrollmentIntent, validTarget } from '../../transport.js'
import { deviceEnrollmentRequestId, verifyDeviceEnrollmentOffer } from '../../generated/vibeface_secure.js'
import type { EnrollmentOfferDescriptor, VerifiedEnrollmentOffer } from '../../generated/vibeface_secure.js'
import { parseAuthorizationTrust } from './authorization-trust'
import { array, integer, record, same, strings, text } from './node'
import { parseEnrollmentReference } from './enrollment-reference'
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
export type SavedEnrollmentRequest = {
  version: 1
  reference: EnrollmentReference
  offerJson: string
  unsignedIntent: Record<string, unknown>
  requestId: string
  nonce: string | null
}

export function parseSavedEnrollmentRequest(value: unknown): SavedEnrollmentRequest {
  const input = record(value, '已保存的设备申请'), unsignedIntent = record(input.unsignedIntent)
  if (input.version !== 1) throw new Error('已保存的设备申请版本无效，请保留数据并核对。')
  const reference = parseEnrollmentReference(input.reference, integer(unsignedIntent.certificate_not_before_ms))
  const offerJson = text(input.offerJson), requestId = deviceEnrollmentRequestId(reference.offer_hash, JSON.stringify(unsignedIntent))
  if (input.requestId !== requestId) throw new Error('已保存的设备申请内容与原编号不同。')
  const checked = verifyDeviceEnrollmentOffer(offerJson, integer(unsignedIntent.certificate_not_before_ms))
  try { inspectEnrollmentOffer(checked, reference) } finally { checked.free() }
  return { version: 1, reference, offerJson, unsignedIntent, requestId, nonce: input.nonce === null ? null : validTarget(input.nonce) }
}

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
    || !same(JSON.parse(descriptor.entryJson()), { endpoint: reference.endpoint, node_public_key: reference.node_public_key, relay: reference.relay })) throw new Error('取得的设备邀请与扫码时固定的账户、节点或摘要不同。')
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

export async function requestDeviceEnrollment(input: {
  reference: EnrollmentReference; ownPublicKey: string; call: EnrollmentCall; signal: AbortSignal
  saved: unknown; save: (request: SavedEnrollmentRequest) => void
}) {
  const { reference, ownPublicKey, call, signal } = input
  signal.throwIfAborted()
  let saved = input.saved === null ? null : parseSavedEnrollmentRequest(input.saved)
  if (saved && (!same(saved.reference, reference) || saved.unsignedIntent.new_device_public_key !== ownPublicKey)) throw new Error('此设备已有待核对的申请，请使用原邀请继续。')
  const lookup = { account_id: reference.account_id, offer_hash: reference.offer_hash }
  let response: Record<string, unknown> | null = null
  let offer: VerifiedEnrollmentOffer | null = null
  try {
    if (saved) {
      response = record(await call('enrollment_status', { ...lookup, request_id: saved.requestId }))
      signal.throwIfAborted()
      if (response.status === 'not_found') {
        if (response.account_id !== reference.account_id || response.offer_hash !== reference.offer_hash || response.request_id !== saved.requestId) throw new Error('节点未确认原申请不存在，不会重新提交。')
        response = null
      }
    }
    if (response?.status !== 'completed') {
      const published = record(await call('enrollment_offer_get', lookup))
      signal.throwIfAborted()
      const offerJson = JSON.stringify(record(published.offer))
      if (saved && !same(JSON.parse(offerJson), JSON.parse(saved.offerJson))) throw new Error('节点返回的邀请与原申请不同。')
      offer = await readVerifiedEnrollmentOffer(offerJson, reference, ownPublicKey, call, signal)
      if (!saved) {
        const unsignedIntent = record(JSON.parse(createClientEnrollmentIntent(offer)))
        if (unsignedIntent.new_device_public_key !== ownPublicKey) throw new Error('设备接入意图未绑定当前浏览器的独立公钥。')
        saved = { version: 1, reference, offerJson, unsignedIntent, requestId: deviceEnrollmentRequestId(reference.offer_hash, JSON.stringify(unsignedIntent)), nonce: null }
        input.save(saved)
      }
    }
    if (!saved) throw new Error('缺少原设备申请，无法继续。')
    signal.throwIfAborted()
    if (!response) response = record(await call('enrollment_request', { ...lookup, new_device_public_key: ownPublicKey, intent: saved.unsignedIntent }))
    signal.throwIfAborted()
    const status = parseEnrollmentRequest(response)
    if (response.offer_hash !== reference.offer_hash || status.requestId !== saved.requestId || status.newDevicePublicKey !== ownPublicKey
      || !same(status.unsignedIntent, saved.unsignedIntent) || saved.nonce !== null && saved.nonce !== status.nonce) throw new Error('节点未确认原设备申请的内容。')
    saved = { ...saved, nonce: status.nonce }; input.save(saved)
    const descriptor = verifyDeviceEnrollmentOffer(saved.offerJson, integer(saved.unsignedIntent.certificate_not_before_ms))
    let checked: ReturnType<typeof inspectEnrollmentOffer>
    try { checked = inspectEnrollmentOffer(descriptor, reference) } finally { descriptor.free() }
    const verifiedOffer = offer
    offer = null
    let released = false
    return {
      ...checked, reference, offerJson: saved.offerJson, requestId: saved.requestId, nonce: status.nonce, unsignedIntent: saved.unsignedIntent, offer: verifiedOffer, status,
      release() { if (!released) { released = true; verifiedOffer?.free() } },
    }
  } finally { offer?.free() }
}

export function verifyEnrollmentStatus(value: unknown, original: Awaited<ReturnType<typeof requestDeviceEnrollment>>) {
  const request = parseEnrollmentRequest(value)
  if (record(value).offer_hash !== original.reference.offer_hash || request.requestId !== original.requestId || request.nonce !== original.nonce || request.newDevicePublicKey !== original.unsignedIntent.new_device_public_key
    || !same(request.unsignedIntent, original.unsignedIntent)
    || request.intent && !same(record(request.intent.payload), original.unsignedIntent)) throw new Error('节点返回的接入状态与原设备密钥或原接入意图不同。')
  return request
}
