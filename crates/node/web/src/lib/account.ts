import { buildClientAccountRecoveryPending, prepareClientAccountCreation, prepareClientAccountRecovery, restoreClientAccountRecovery, validEndpoint, validTarget } from '../../transport.js'
import { Identity, beginRecoveryContextImport, buildAccountRecoveryMaterial, deriveAuthorizationId, finishAccountCreation, finishAccountRecovery, inspectAccountRecoveryMaterial, recoveryOpen, recoverySeal, verifyNodeClaim } from '../../generated/vibeface_secure.js'
import type { PendingAccountRecovery, VerifiedClientAuthorization } from '../../generated/vibeface_secure.js'
import { parseAuthorizationSelection, parseAuthorizationTrust } from './authorization'
import { typedId } from './authorization-trust'
import { array, record, strings, text } from './node'
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
  if (!file.size || file.size > 2 * 1024 * 1024 + 4096) throw new Error('选择不超过 2 MiB 的加密账户恢复包。')
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

type RecoveryCall = (method: string, params: Record<string, unknown>) => Promise<unknown>

async function readRecoveryPolicyContext(input: {
  material: string
  requestJson: string
  snapshot: unknown
  signal: AbortSignal
  call: RecoveryCall
}) {
  let importer: ReturnType<typeof beginRecoveryContextImport> | null = null
  try {
    input.signal.throwIfAborted()
    importer = beginRecoveryContextImport(input.material, input.requestJson, JSON.stringify(input.snapshot), Date.now())
    const payload = record(record(input.snapshot).payload)
    const reference = { snapshot_id: text(payload.snapshot_id), request_hash: typedId(payload.request_hash, 'h2_') }
    let cursor: string | null = null
    for (;;) {
      const response = record(await input.call('recovery_context_page', { ...reference, cursor }))
      input.signal.throwIfAborted()
      const progress = record(JSON.parse(importer.appendPage(JSON.stringify(response.page), Date.now())))
      if (progress.complete === true) break
      const next = typedId(progress.cursor, 'h2_')
      if (next === cursor) throw new Error('恢复政策分页没有推进原请求游标。')
      cursor = next
    }
    const response = record(await input.call('recovery_context_witness', { ...reference, cursor: null }))
    input.signal.throwIfAborted()
    const witnessJson = JSON.stringify(record(response.witness))
    const completed = importer
    importer = null
    return completed.finish(witnessJson, Date.now())
  } finally { importer?.free() }
}

function pendingRecoveryReview(pending: PendingAccountRecovery) {
  const inspected = inspectRecoveryMaterial(pending.recoveryMaterialJson())
  const request = record(JSON.parse(pending.commitRequestJson()))
  const grants = array(request.journal_delta).map(value => record(value)).filter(entry => entry.kind === 'grant').map(entry => record(record(entry.value).payload))
  const service = deriveAuthorizationId('service', inspected.entry.nodePublicKey)
  const matches = grants.filter(grant => {
    const grantee = record(grant.grantee), scope = record(grant.scope)
    if (grantee.kind !== 'actor' || scope.kind !== 'exact') return false
    const actor = record(grantee.value)
    return actor.kind === 'service' && actor.id === service
  })
  if (matches.length !== 1) throw new Error('待提交恢复包没有唯一的原 Node 服务授权。')
  const grant = matches[0], scope = record(grant.scope), target = record(scope.value)
  if (grant.remaining_delegations !== 0 || grant.owner_space !== inspected.scope.space_id || target.space !== inspected.scope.space_id) throw new Error('待提交恢复包的 Node 授权范围不一致。')
  const serviceGrant = { projectId: typedId(target.resource, 'res2_'), actions: strings(grant.actions) }
  const selection = parseAuthorizationSelection({ resourceId: serviceGrant.projectId, grantLeaf: request.grant_leaf,
    audience: { device: deriveAuthorizationId('device', inspected.entry.nodePublicKey), service } })
  return { ...inspected, serviceGrant, selection }
}

function accountRecoveryFlow(input: {
  pending: PendingAccountRecovery
  password: string
  signal: AbortSignal
  call: RecoveryCall
  restored: boolean
}) {
  const review = pendingRecoveryReview(input.pending)
  type State = { kind: 'prepared' | 'unknown' } | { kind: 'committed'; receipt: Record<string, unknown> } | { kind: 'confirmed' } | { kind: 'released' }
  let state: State = { kind: input.restored ? 'unknown' : 'prepared' }
  let pending: PendingAccountRecovery | null = input.pending, password = input.password
  let bytes: Uint8Array = new Uint8Array()
  let running = false, releaseRequested = false
  input.password = ''
  const seal = (material: string) => {
    const cleartext = encoder.encode(material)
    try { return recoverySeal(cleartext, password) } finally { cleartext.fill(0) }
  }
  const check = () => { input.signal.throwIfAborted(); if (releaseRequested || state.kind === 'released') throw new Error('原账户恢复流程已关闭。') }
  const dispose = () => {
    input.signal.removeEventListener('abort', release)
    pending?.free(); pending = null; password = ''; bytes.fill(0); state = { kind: 'released' }
  }
  function release() { releaseRequested = true; if (!running) dispose() }
  try { bytes = seal(pending.pendingMaterialJson()) }
  catch (error) { password = ''; bytes.fill(0); throw error }
  input.signal.addEventListener('abort', release, { once: true })
  return {
    scope: review.scope, trust: review.trust, serviceGrant: review.serviceGrant, selection: review.selection,
    entry: review.entry,
    get bytes() { check(); return bytes },
    get phase() { return state.kind },
    get submitted() { return state.kind !== 'prepared' },
    get committed() { return state.kind === 'committed' || state.kind === 'confirmed' },
    release,
    async commit(saved: boolean) {
      check()
      if (running) throw new Error('原账户恢复请求仍在等待结果。')
      if (!saved) throw new Error('请先下载当前待提交恢复包，并确认已妥善保存。')
      if (state.kind === 'committed' || state.kind === 'confirmed') return
      if (!pending || state.kind === 'released') throw new Error('原账户恢复流程已关闭。')
      const original = pending
      running = true
      try {
        if (state.kind === 'unknown') {
          const status = record(await input.call('recovery_status', record(JSON.parse(original.statusRequestJson()))))
          check()
          if (status.state === 'committed') { state = { kind: 'committed', receipt: record(status.receipt) }; return }
          if (status.state !== 'not_committed') throw new Error('原恢复操作的状态无法确认。请保留待提交恢复包继续对账。')
        }
        state = { kind: 'unknown' }
        const result = record(await input.call('recover', record(JSON.parse(original.commitRequestJson()))))
        check()
        state = { kind: 'committed', receipt: record(result.receipt) }
      } finally { running = false; if (releaseRequested) dispose() }
    },
    confirm(authorization: VerifiedClientAuthorization) {
      check()
      if (running || state.kind !== 'committed' || !pending) throw new Error('请先核对原恢复操作已提交，再取得当前有限授权。')
      const confirmed = pending.confirmCompletion(JSON.stringify(state.receipt), authorization, Date.now())
      let handle: VerifiedClientAuthorization | null = null, finalBytes: Uint8Array | null = null
      try {
        handle = confirmed.authorization()
        finalBytes = seal(confirmed.recoveryMaterialJson())
        check()
        bytes.fill(0); bytes = finalBytes; finalBytes = null; password = ''; state = { kind: 'confirmed' }
        const result = handle; handle = null
        pending.free(); pending = null
        return result
      } finally { finalBytes?.fill(0); handle?.free(); confirmed.free() }
    },
  }
}

export async function recoverAccount(input: {
  material: string
  requestJson: string
  snapshot: unknown
  nodePublicKey: string
  endpoint: string
  password: string
  signal: AbortSignal
  call: RecoveryCall
}) {
  let root: Identity | null = Identity.generate()
  let context: Awaited<ReturnType<typeof readRecoveryPolicyContext>> | null = null
  let prepared: ReturnType<typeof prepareClientAccountRecovery> | null = null
  let recovered: ReturnType<typeof finishAccountRecovery> | null = null
  let pending: PendingAccountRecovery | null = null
  try {
    const original = inspectRecoveryMaterial(input.material)
    if (original.entry.nodePublicKey !== validTarget(input.nodePublicKey)) throw new Error('账户恢复目标与原恢复包的独立 Node 公钥不一致。')
    context = await readRecoveryPolicyContext(input)
    input.signal.throwIfAborted()
    prepared = prepareClientAccountRecovery(input.material, context, root, Date.now() + 365 * 24 * 60 * 60 * 1000)
    context.free(); context = null
    const proof = record(await input.call('recovery_node_possession', record(JSON.parse(prepared.nodeRequestJson()))))
    input.signal.throwIfAborted()
    recovered = finishAccountRecovery(prepared, root, text(proof.proof), Date.now())
    pending = buildClientAccountRecoveryPending(prepared, recovered, root, input.material, validEndpoint(input.endpoint))
    const flow = accountRecoveryFlow({ pending, password: input.password, signal: input.signal, call: input.call, restored: false })
    pending = null
    return flow
  } finally {
    pending?.free(); recovered?.free(); prepared?.free(); context?.free(); root?.free()
    input.material = ''; input.password = ''
  }
}

export async function restoreAccountRecovery(input: {
  file: File
  password: string
  newPassword?: string
  signal: AbortSignal
  call: RecoveryCall
  nodePublicKey?: string
  endpoint?: string
}) {
  let cleartext: Uint8Array | null = null, pending: PendingAccountRecovery | null = null, material = ''
  try {
    input.signal.throwIfAborted()
    if (!input.file.size || input.file.size > 2 * 1024 * 1024 + 4096) throw new Error('选择不超过 2 MiB 的待提交账户恢复包。')
    cleartext = recoveryOpen(new Uint8Array(await input.file.arrayBuffer()), input.password)
    input.signal.throwIfAborted()
    material = decoder.decode(cleartext); cleartext.fill(0); cleartext = null
    pending = restoreClientAccountRecovery(material)
    const review = pendingRecoveryReview(pending)
    if (input.nodePublicKey !== undefined && validTarget(input.nodePublicKey) !== review.entry.nodePublicKey) throw new Error('恢复入口的独立 Node 公钥与原待提交操作不一致。')
    if (input.endpoint !== undefined && validEndpoint(input.endpoint) !== review.entry.endpoint) throw new Error('请使用原待提交恢复包保存的 Node 入口。')
    const flow = accountRecoveryFlow({ pending, password: input.newPassword ?? input.password, signal: input.signal, call: input.call, restored: true })
    pending = null
    return flow
  } finally { pending?.free(); cleartext?.fill(0); material = ''; input.password = ''; input.newPassword = '' }
}
