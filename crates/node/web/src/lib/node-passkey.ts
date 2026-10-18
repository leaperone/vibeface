import { verifyNodePasskeyPending, verifyNodePasskeyState, verifyPasskeyAssertionFromEnrollmentOffer } from '../../generated/vibeface_secure.js'
import type { VerifiedEnrollmentOffer } from '../../generated/vibeface_secure.js'
import { record, text } from './node'
import { assertPasskey } from './passkey'

export async function assertNodePasskey(input: {
  pendingResponse: unknown
  signedIntent: unknown
  nodePublicKey: string
  offer: VerifiedEnrollmentOffer
  signal: AbortSignal
  requestPrf?: boolean
  verify(pendingId: string, response: Awaited<ReturnType<typeof assertPasskey>>['response']): Promise<unknown>
}) {
  const rpId = location.hostname, origin = location.origin
  const signedIntentJson = JSON.stringify(record(input.signedIntent, 'Passkey 登记请求'))
  let pending: ReturnType<typeof verifyNodePasskeyPending> | null = null
  let state: ReturnType<typeof verifyNodePasskeyState> | null = null
  let assertion: ReturnType<typeof verifyPasskeyAssertionFromEnrollmentOffer> | null = null
  let prf: Uint8Array | null = null
  let salt: Uint8Array | null = null
  const release = () => {
    assertion?.free(); assertion = null
    state?.free(); state = null
    pending?.free(); pending = null
    prf?.fill(0); prf = null
    salt?.fill(0); salt = null
  }
  input.signal.addEventListener('abort', release, { once: true })
  try {
    input.signal.throwIfAborted()
    const response = record(input.pendingResponse, 'Passkey 待验证请求')
    pending = verifyNodePasskeyPending(JSON.stringify(record(response.signed_pending)), input.nodePublicKey, signedIntentJson, rpId, origin, Date.now())
    const pendingId = pending.envelopeHash()
    const pendingNonce = text(record(record(JSON.parse(pending.pendingJson())).envelope).nonce)
    if (input.requestPrf) salt = crypto.getRandomValues(new Uint8Array(32))
    const browser = await assertPasskey(JSON.parse(pending.optionsJson()), input.signal, salt ?? undefined)
    prf = browser.prf
    input.signal.throwIfAborted()
    const verified = record(await input.verify(pendingId, browser.response), 'Passkey 验证响应')
    input.signal.throwIfAborted()
    if (!pending) throw new Error('Passkey 验证已取消。')
    const proofJson = JSON.stringify(record(verified.proof, 'Passkey 证明'))
    state = verifyNodePasskeyState(JSON.stringify(record(verified.signed_state)), pending, proofJson, JSON.stringify(browser.response), Date.now())
    assertion = verifyPasskeyAssertionFromEnrollmentOffer(proofJson, state, input.offer, pendingId, rpId, origin, Date.now())
    input.signal.throwIfAborted()
    const proof = record(JSON.parse(proofJson), '已验证的 Passkey 证明')
    if (prf && salt) {
      const localPrf = prf, prfSalt = salt
      prf = null; salt = null
      return { proof, pendingId, pendingNonce, credentialId: browser.response.id, localPrf, prfSalt }
    }
    return { proof, pendingId, pendingNonce, credentialId: browser.response.id, localPrf: null, prfSalt: null }
  } finally {
    input.signal.removeEventListener('abort', release)
    release()
  }
}
