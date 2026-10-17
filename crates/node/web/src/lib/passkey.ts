import { array, record, text } from './node'

type RegistrationResponse = {
  id: string; rawId: string; type: 'public-key'
  response: { clientDataJSON: string; attestationObject: string; transports: string[] }
  clientExtensionResults: Record<string, never>
}
type AssertionResponse = {
  id: string; rawId: string; type: 'public-key'
  response: { clientDataJSON: string; authenticatorData: string; signature: string; userHandle: string | null }
  clientExtensionResults: Record<string, never>
}

export function encodeBase64Url(value: ArrayBuffer) {
  return btoa(String.fromCharCode(...new Uint8Array(value))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

export function decodeBase64Url(value: unknown, maximum = 8192): ArrayBuffer {
  const input = text(value, 'Passkey 数据')
  if (!/^[A-Za-z0-9_-]+$/.test(input) || input.length > Math.ceil(maximum * 4 / 3)) throw new Error('Passkey 数据编码无效。')
  let bytes: Uint8Array<ArrayBuffer>
  try { bytes = Uint8Array.from(atob(input.replace(/-/g, '+').replace(/_/g, '/')), char => char.charCodeAt(0)) }
  catch { throw new Error('Passkey 数据编码无效。') }
  if (!bytes.length || bytes.length > maximum || encodeBase64Url(bytes.buffer) !== input) throw new Error('Passkey 数据编码无效。')
  return bytes.buffer
}

function requirePasskey(rpId: unknown) {
  if (!globalThis.isSecureContext || !globalThis.PublicKeyCredential || !navigator.credentials) throw new Error('此浏览器入口不支持 Passkey。请选择口令仓库或临时会话。')
  if (rpId !== location.hostname) throw new Error('Passkey 的站点与当前可信客户端不同，已停止。')
}
function descriptors(value: unknown): PublicKeyCredentialDescriptor[] {
  return array(value).map(value => {
    const descriptor = record(value)
    if (descriptor.type !== 'public-key') throw new Error('Passkey 凭证类型无效。')
    return { type: 'public-key', id: decodeBase64Url(descriptor.id, 1024) }
  })
}
function prfInput(salt?: Uint8Array): AuthenticationExtensionsClientInputs | undefined {
  if (!salt) return undefined
  if (salt.length !== 32) throw new Error('本地 Passkey 保护盐格式无效。')
  return { prf: { eval: { first: new Uint8Array(salt).buffer } } }
}
function localPrf(credential: PublicKeyCredential) {
  const first = credential.getClientExtensionResults().prf?.results?.first
  if (!first) return null
  const bytes = ArrayBuffer.isView(first) ? new Uint8Array(first.buffer, first.byteOffset, first.byteLength) : new Uint8Array(first)
  try {
    if (bytes.length !== 32) throw new Error('Passkey 返回的本地保护密钥长度无效，未保存设备配置。')
    return bytes.slice()
  } finally { bytes.fill(0) }
}
function credentialId(credential: PublicKeyCredential) {
  const id = encodeBase64Url(credential.rawId)
  if (credential.id !== id) throw new Error('Passkey 返回的凭证编号不一致。')
  return id
}

export async function createPasskey(value: unknown, signal: AbortSignal, prfSalt?: Uint8Array) {
  const input = record(record(value).publicKey)
  const rp = record(input.rp), user = record(input.user), selection = record(input.authenticatorSelection)
  requirePasskey(rp.id)
  if (input.attestation !== 'none' || selection.residentKey !== 'required' || selection.requireResidentKey !== true || selection.userVerification !== 'required') throw new Error('Passkey 必须使用可发现凭证并验证本人。')
  const algorithms = array(input.pubKeyCredParams).map(value => {
    const algorithm = record(value)
    if (algorithm.type !== 'public-key' || algorithm.alg !== -7) throw new Error('Passkey 算法不受支持。')
    return { type: 'public-key', alg: -7 } satisfies PublicKeyCredentialParameters
  })
  if (algorithms.length !== 1) throw new Error('Passkey 算法配置无效。')
  const options: PublicKeyCredentialCreationOptions = {
    rp: { id: text(rp.id), name: text(rp.name) },
    user: { id: decodeBase64Url(user.id, 64), name: text(user.name), displayName: text(user.displayName) },
    challenge: decodeBase64Url(input.challenge, 64), pubKeyCredParams: algorithms,
    timeout: 120000, attestation: 'none', authenticatorSelection: { residentKey: 'required', requireResidentKey: true, userVerification: 'required' },
    excludeCredentials: descriptors(input.excludeCredentials), extensions: prfInput(prfSalt),
  }
  const credential = await navigator.credentials.create({ publicKey: options, signal })
  if (!(credential instanceof PublicKeyCredential) || !(credential.response instanceof AuthenticatorAttestationResponse)) throw new Error('未取得完整的 Passkey 创建响应。')
  const id = credentialId(credential)
  const response: RegistrationResponse = {
    id, rawId: id, type: 'public-key',
    response: { clientDataJSON: encodeBase64Url(credential.response.clientDataJSON), attestationObject: encodeBase64Url(credential.response.attestationObject), transports: credential.response.getTransports?.() ?? [] },
    clientExtensionResults: {},
  }
  return { response, prf: localPrf(credential) }
}

export async function assertPasskey(value: unknown, signal: AbortSignal, prfSalt?: Uint8Array) {
  const input = record(record(value).publicKey)
  requirePasskey(input.rpId)
  if (input.userVerification !== 'required') throw new Error('Passkey 必须验证本人。')
  const options: PublicKeyCredentialRequestOptions = {
    challenge: decodeBase64Url(input.challenge, 64), rpId: text(input.rpId), timeout: 120000,
    userVerification: 'required', allowCredentials: descriptors(input.allowCredentials), extensions: prfInput(prfSalt),
  }
  const credential = await navigator.credentials.get({ publicKey: options, signal })
  if (!(credential instanceof PublicKeyCredential) || !(credential.response instanceof AuthenticatorAssertionResponse)) throw new Error('未取得完整的 Passkey 验证响应。')
  const id = credentialId(credential)
  const response: AssertionResponse = {
    id, rawId: id, type: 'public-key',
    response: {
      clientDataJSON: encodeBase64Url(credential.response.clientDataJSON), authenticatorData: encodeBase64Url(credential.response.authenticatorData), signature: encodeBase64Url(credential.response.signature),
      userHandle: credential.response.userHandle ? encodeBase64Url(credential.response.userHandle) : null,
    },
    clientExtensionResults: {},
  }
  return { response, prf: localPrf(credential) }
}
