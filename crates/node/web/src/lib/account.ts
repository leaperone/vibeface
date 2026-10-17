import { prepareClientAccountCreation, prepareClientAccountRecovery, validEndpoint, validTarget } from '../../transport.js'
import { Identity, buildAccountRecoveryMaterial, buildRecoveredAccountRecoveryMaterial, finishAccountCreation, finishAccountRecovery, inspectAccountRecoveryMaterial, recoveryOpen, recoverySeal, verifyNodeClaim } from '../../generated/vibeface_secure.js'
import { parseAuthorizationTrust } from './authorization'
import { record, strings, text } from './node'
import type { ConnectionCard } from './node'
import { parseIdentityScope } from './scope'

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })

function serviceGrantReview(created: ReturnType<typeof finishAccountCreation>) {
  const grant = record(record(JSON.parse(created.serviceGrantJson())).payload)
  const scope = record(grant.scope)
  const target = record(scope.value)
  if (scope.kind !== 'exact' || grant.remaining_delegations !== 0 || grant.owner_space !== target.space) throw new Error('Node 授权必须限于一个项目且不可继续转授。')
  return { projectId: text(target.resource), actions: strings(grant.actions) }
}

export function inspectRecoveryMaterial(material: string) {
  const verified = record(JSON.parse(inspectAccountRecoveryMaterial(material, Date.now())))
  const scope = parseIdentityScope(verified.scope)
  const entry = record(verified.entry)
  return {
    scope,
    trust: parseAuthorizationTrust({ kind: 'policy', accountId: scope.account_id, spaceId: scope.space_id, pins: verified.pins }),
    entry: { endpoint: validEndpoint(text(entry.endpoint)), nodePublicKey: validTarget(entry.node_public_key) },
  }
}

export function sealAccountPackage(material: string, password: string) {
  inspectRecoveryMaterial(material)
  const bytes = encoder.encode(material)
  try { return recoverySeal(bytes, password) }
  finally { bytes.fill(0) }
}

export async function openAccountPackage(file: File, password: string) {
  if (!file.size || file.size > 256 * 1024) throw new Error('选择不超过 256 KiB 的加密账户恢复包。')
  let cleartext: Uint8Array
  try { cleartext = recoveryOpen(new Uint8Array(await file.arrayBuffer()), password) }
  catch { throw new Error('无法解锁账户恢复包。请核对口令，并使用未修改的原始文件。') }
  let material = ''
  try {
    material = decoder.decode(cleartext)
    const verified = inspectRecoveryMaterial(material)
    return { ...verified, get material() { return material }, release() { material = '' } }
  } finally { cleartext.fill(0) }
}

export async function createAccount(input: {
  card: Extract<ConnectionCard, { kind: 'claim' }>
  password: string
  signal: AbortSignal
  call(method: string, params: Record<string, unknown>): Promise<unknown>
}) {
  let root: Identity | null = Identity.generate()
  let recovery: Identity | null = Identity.generate()
  let claim: ReturnType<typeof verifyNodeClaim> | null = null
  let prepared: ReturnType<typeof prepareClientAccountCreation> | null = null
  let created: ReturnType<typeof finishAccountCreation> | null = null
  const release = () => {
    created?.free(); created = null
    prepared?.free(); prepared = null
    claim?.free(); claim = null
    root?.free(); root = null
    recovery?.free(); recovery = null
    input.password = ''
  }
  input.signal.addEventListener('abort', release, { once: true })
  try {
    input.signal.throwIfAborted()
    const response = record(await input.call('node_claim', { capability: input.card.capability }))
    input.signal.throwIfAborted()
    claim = verifyNodeClaim(JSON.stringify(record(response.claim)), input.card.nodePublicKey, input.card.claimNonce, Date.now())
    if (!root || !recovery) throw new Error('账户创建已取消。')
    prepared = prepareClientAccountCreation(root, recovery, claim, Date.now() + 365 * 24 * 60 * 60 * 1000)
    const possession = record(await input.call('node_possession', record(JSON.parse(prepared.nodeRequestJson()))))
    input.signal.throwIfAborted()
    if (!prepared || !root || !recovery) throw new Error('账户创建已取消。')
    created = finishAccountCreation(prepared, root, text(possession.proof), Date.now())
    const scope = parseIdentityScope(JSON.parse(created.scopeJson()))
    const trust = parseAuthorizationTrust({ kind: 'policy', accountId: scope.account_id, spaceId: scope.space_id, pins: JSON.parse(created.pinsJson()) })
    const authorization = record(JSON.parse(created.bundleJson()))
    const serviceGrant = serviceGrantReview(created)
    const material = buildAccountRecoveryMaterial(created, root, recovery, input.card.entry.address, Date.now())
    const bytes = sealAccountPackage(material, input.password)
    return { scope, trust, authorization, serviceGrant, bytes }
  } finally {
    input.signal.removeEventListener('abort', release)
    release()
  }
}

export async function recoverAccount(input: {
  material: string
  latestBundle: unknown
  claim: unknown
  nodePublicKey: string
  claimNonce: string
  endpoint: string
  password: string
  signal: AbortSignal
  possession(params: Record<string, unknown>): Promise<unknown>
}) {
  let root: Identity | null = Identity.generate()
  let claim: ReturnType<typeof verifyNodeClaim> | null = null
  let prepared: ReturnType<typeof prepareClientAccountRecovery> | null = null
  let recovered: ReturnType<typeof finishAccountRecovery> | null = null
  const release = () => {
    recovered?.free(); recovered = null
    prepared?.free(); prepared = null
    claim?.free(); claim = null
    root?.free(); root = null
    input.password = ''; input.material = ''
  }
  input.signal.addEventListener('abort', release, { once: true })
  try {
    input.signal.throwIfAborted()
    claim = verifyNodeClaim(JSON.stringify(input.claim), input.nodePublicKey, input.claimNonce, Date.now())
    if (!root) throw new Error('账户恢复已取消。')
    prepared = prepareClientAccountRecovery(input.material, JSON.stringify(input.latestBundle), root, claim, Date.now() + 365 * 24 * 60 * 60 * 1000)
    const proof = record(await input.possession(record(JSON.parse(prepared.nodeRequestJson()))))
    input.signal.throwIfAborted()
    if (!root || !prepared) throw new Error('账户恢复已取消。')
    recovered = finishAccountRecovery(prepared, root, text(proof.proof), Date.now())
    const scope = parseIdentityScope(JSON.parse(recovered.scopeJson()))
    const trust = parseAuthorizationTrust({ kind: 'policy', accountId: scope.account_id, spaceId: scope.space_id, pins: JSON.parse(recovered.pinsJson()) })
    const authorization = record(JSON.parse(recovered.bundleJson()))
    const serviceGrant = serviceGrantReview(recovered)
    const commit = record(JSON.parse(prepared.commitJson(recovered)))
    const material = buildRecoveredAccountRecoveryMaterial(recovered, root, input.material, input.endpoint, Date.now())
    const bytes = sealAccountPackage(material, input.password)
    return { scope, trust, authorization, serviceGrant, commit, bytes }
  } finally {
    input.signal.removeEventListener('abort', release)
    release()
  }
}
