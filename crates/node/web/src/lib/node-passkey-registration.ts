import { verifyNodePasskeyPending, verifyPasskeyRegistration } from '../../generated/vibeface_secure.js'
import { record } from './node'
import { createPasskey } from './passkey'

export async function registerNodePasskey(input: {
  pendingResponse: unknown
  signedIntent: unknown
  nodePublicKey: string
  signal: AbortSignal
  requestPrf?: boolean
}) {
  const rpId = location.hostname, origin = location.origin
  const signedIntentJson = JSON.stringify(record(input.signedIntent, 'Passkey 登记请求'))
  let pending: ReturnType<typeof verifyNodePasskeyPending> | null = null
  let registration: ReturnType<typeof verifyPasskeyRegistration> | null = null
  let prf: Uint8Array | null = null
  let salt: Uint8Array | null = null
  const release = () => {
    input.signal.removeEventListener('abort', release)
    registration?.free(); registration = null
    pending?.free(); pending = null
    prf?.fill(0); prf = null
    salt?.fill(0); salt = null
  }
  input.signal.addEventListener('abort', release, { once: true })
  try {
    input.signal.throwIfAborted()
    const response = record(input.pendingResponse, 'Passkey 待登记请求')
    pending = verifyNodePasskeyPending(JSON.stringify(record(response.signed_pending)), input.nodePublicKey, signedIntentJson, rpId, origin, Date.now())
    const pendingId = pending.envelopeHash()
    if (input.requestPrf) salt = crypto.getRandomValues(new Uint8Array(32))
    const browser = await createPasskey(JSON.parse(pending.optionsJson()), input.signal, salt ?? undefined)
    prf = browser.prf
    input.signal.throwIfAborted()
    if (!pending) throw new Error('Passkey 登记已取消。')
    registration = verifyPasskeyRegistration(pending.pendingJson(), JSON.stringify(browser.response), pendingId, rpId, origin, Date.now())
    pending.free(); pending = null
    if (!prf || !salt) { prf?.fill(0); prf = null; salt?.fill(0); salt = null }
    input.signal.throwIfAborted()
    return { pendingId, response: browser.response, registration, credentialId: browser.response.id, localPrf: prf, prfSalt: salt, release }
  } catch (error) {
    release()
    throw error
  }
}
