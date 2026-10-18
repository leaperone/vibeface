import { buildClientCrossNodeRecoveryRequest, connectDevice, prepareClientCrossNodeAccountRecovery, restoreClientCrossNodeRecovery, validEndpoint } from '../../transport.js'
import type { DeviceChannel } from '../../transport.js'
import { beginCrossNodeRecoveryContextImport, deriveAuthorizationId, finishCrossNodeAccountRecovery, finishCrossNodeRecoveryCommit, Identity, inspectCrossNodeRecoverySources, nodeFingerprint, recoveryOpen, recoverySeal, verifyNodeClaim } from '../../generated/vibeface_secure.js'
import { openAccountPackage } from './account'
import { parseAuthorizationSelection, parseAuthorizationTrust } from './authorization'
import { counter, typedId } from './authorization-trust'
import type { AuthorizationTrust } from './authorization'
import { array, integer, parseInfo, record, same, strings, text } from './node'
import type { ConnectionCard, ConnectionEntry } from './node'
import { parseIdentityScope } from './scope'
import type { IdentityScope } from './scope'

type ResourceRef = { space: string; resource: string }
export type CrossNodeRecoveryReview = {
  accountId: string
  spaceId: string
  serviceGrant: { projectId: string; actions: string[] }
  sourceRead: { nodePublicKey: string; projects: ResourceRef[]; actions: string[]; expiresAt: number }
  targetNodePublicKey: string
}
export type CrossNodeSourceReadSelection = {
  project: ResourceRef
  targetPublicKey: string
  grantLeaf: string
  scope: IdentityScope
  trust: AuthorizationTrust
}
export type CrossNodeRecoveryResult = {
  scope: IdentityScope | null
  trust: AuthorizationTrust
  sourceReadSelections: CrossNodeSourceReadSelection[]
  selection: ReturnType<typeof parseAuthorizationSelection>
}
type RootMaterial = { root: Identity; opened: Awaited<ReturnType<typeof openAccountPackage>> }
type Prepared = ReturnType<typeof prepareClientCrossNodeAccountRecovery>
type Commit = ReturnType<typeof finishCrossNodeAccountRecovery>
type Verified = ReturnType<typeof finishCrossNodeRecoveryCommit>
type Importer = ReturnType<typeof beginCrossNodeRecoveryContextImport>
type Context = ReturnType<Importer['finish']>
type ReadBinding = { snapshot_id: string; request_hash: string; snapshot_hash: string }
type Checkpoint = { request: Record<string, unknown>; snapshot: Record<string, unknown>; witness: Record<string, unknown>; context: Record<string, unknown> }
type Reading = RootMaterial & { importer: Importer; requestJson: string; snapshot: Record<string, unknown>; binding: ReadBinding; certificateExpiresAt: number }
type Stage =
  | RootMaterial & { kind: 'request'; requestJson: string; certificateExpiresAt: number }
  | Reading & { kind: 'reading'; cursor: string | null }
  | Reading & { kind: 'page'; page: Record<string, unknown>; nextCursor: string | null; complete: boolean; pageCount: number }
  | Reading & { kind: 'witness' }
  | RootMaterial & { kind: 'finishing'; context: Context; checkpoint: Checkpoint; certificateExpiresAt: number }
  | RootMaterial & { kind: 'possession'; prepared: Prepared; request: Record<string, unknown>; targetProof: string | null; sourceProof: string | null; checkpoint: Checkpoint }
  | RootMaterial & { kind: 'sealing'; commit: Commit; checkpoint: Checkpoint }
  | { kind: 'prepared'; commit: Commit; request: Record<string, unknown>; checkpoint: Checkpoint }
  | { kind: 'committed'; verified: Verified; checkpoint: Checkpoint }
  | { kind: 'installing' | 'install_unknown' | 'installed'; result: CrossNodeRecoveryResult; installation: Record<string, unknown>; finalBytes: Uint8Array; checkpoint: Checkpoint }
  | { kind: 'released' }
type Endpoint = { entry: ConnectionEntry; publicKey: string; channel: DeviceChannel | null }
type Endpoints = { source: Endpoint; target: Endpoint }
const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })

function resourceRef(value: unknown): ResourceRef {
  const ref = record(value)
  return { space: text(ref.space), resource: text(ref.resource) }
}
function publicReview(commit: Commit): CrossNodeRecoveryReview {
  const review = record(JSON.parse(commit.publicReviewJson())), scope = parseIdentityScope(review.scope)
  const project = resourceRef(review.personal_project), grant = record(record(review.service_grant).payload)
  const grantScope = record(grant.scope), grantResource = resourceRef(grantScope.value)
  if (grantScope.kind !== 'exact' || !same(grantResource, project) || project.space !== scope.space_id || grant.remaining_delegations !== 0) throw new Error('新 Node 的服务授权必须限于本次新项目且不可转授。')
  return {
    accountId: scope.account_id, spaceId: scope.space_id,
    serviceGrant: { projectId: project.resource, actions: strings(grant.actions) },
    sourceRead: { nodePublicKey: text(review.source_node_public_key), projects: array(review.source_read_resources).map(resourceRef), actions: strings(review.source_read_actions), expiresAt: integer(review.source_read_expires_at_ms) },
    targetNodePublicKey: text(review.target_node_public_key),
  }
}
function recoveryResult(verified: Verified, review: CrossNodeRecoveryReview, sourceKey: string): CrossNodeRecoveryResult {
  const value = JSON.parse(verified.scopeJson()), scope = value === null ? null : parseIdentityScope(value)
  if (scope && (scope.account_id !== review.accountId || scope.space_id !== review.spaceId)) throw new Error('原恢复提交回执属于其他账户或空间。')
  const trust = parseAuthorizationTrust({ kind: 'policy', accountId: review.accountId, spaceId: review.spaceId, pins: JSON.parse(verified.pinsJson()) })
  const sourceReadSelections = array(JSON.parse(verified.sourceReadSelectionsJson())).map(value => {
    const selection = record(value), selectedScope = parseIdentityScope(selection.scope), project = resourceRef(selection.project)
    const targetPublicKey = text(selection.target_public_key), grantLeaf = typedId(selection.grant_leaf, 'h2_')
    if (targetPublicKey !== sourceKey || selectedScope.account_id !== review.accountId || selectedScope.space_id !== project.space) throw new Error('原 Node 的已验证只读项目选择不一致。')
    return { project, targetPublicKey, grantLeaf, scope: selectedScope,
      trust: parseAuthorizationTrust({ kind: 'policy', accountId: selectedScope.account_id, spaceId: selectedScope.space_id, pins: selection.pins }) }
  })
  const installation = record(JSON.parse(verified.installationJson())), request = record(installation.request)
  const selection = parseAuthorizationSelection({ resourceId: review.serviceGrant.projectId, grantLeaf: request.grant_leaf,
    audience: { device: deriveAuthorizationId('device', review.targetNodePublicKey), service: deriveAuthorizationId('service', review.targetNodePublicKey) } })
  return { scope, trust, sourceReadSelections, selection }
}
function endpoint(publicKey: string, address: string): Endpoint {
  return { publicKey, entry: { address: validEndpoint(address), node_id: nodeFingerprint(publicKey), relay: false }, channel: null }
}
function closeEndpoints(endpoints: Endpoints) {
  endpoints.source.channel?.close(); endpoints.target.channel?.close()
}
async function pinnedChannel(target: Endpoint, signal: AbortSignal) {
  signal.throwIfAborted()
  if (target.channel) return target.channel
  let candidate: DeviceChannel | null = null
  candidate = await connectDevice(target.entry, { signal, onClose() { if (target.channel === candidate) target.channel = null } })
  if (candidate.publicKey !== target.publicKey) { candidate.close(); throw new Error('恢复节点的 Noise 公钥与已验证入口不一致。') }
  target.channel = candidate
  try {
    const info = parseInfo(await candidate.request('identity', { method: 'status', params: {} }))
    signal.throwIfAborted()
    if (info.node_id !== target.entry.node_id || info.public_key !== target.publicKey) throw new Error('恢复节点的身份与固定目标不一致。')
    return candidate
  } catch (error) { candidate.close(); if (target.channel === candidate) target.channel = null; throw error }
}
function readBinding(value: unknown): ReadBinding {
  const input = record(value)
  return { snapshot_id: text(input.snapshot_id), request_hash: typedId(input.request_hash, 'h2_'), snapshot_hash: typedId(input.snapshot_hash, 'h2_') }
}
function pendingEnvelope(pending: string, read: Checkpoint) {
  return JSON.stringify({ format: 'vibeface-client-cross-node-recovery-pending', version: 2, pending, read })
}
function parseCheckpoint(value: unknown): Checkpoint {
  const input = record(value)
  return { request: record(input.request), snapshot: record(input.snapshot), witness: record(input.witness), context: record(input.context) }
}
function checkCheckpoint(checkpoint: Checkpoint, commit: Commit) {
  const request = record(JSON.parse(commit.requestJson())), possession = record(request.possession)
  const resume = record(JSON.parse(commit.resumeContextJson()))
  const snapshot = record(checkpoint.snapshot.payload), witness = record(checkpoint.witness.payload)
  if (!same(possession.request, checkpoint.request) || !same(resume.context, checkpoint.context)
    || snapshot.snapshot_id !== checkpoint.context.snapshot_id || snapshot.request_hash !== checkpoint.context.request_hash
    || witness.snapshot_id !== checkpoint.context.snapshot_id || witness.request_hash !== checkpoint.context.request_hash) throw new Error('待提交跨 Node 恢复包更换了原有限读取上下文。')
}
function recoveryFlow(input: { accountId: string; endpoints: Endpoints; stage: Stage; signal: AbortSignal; password: string; bytes?: Uint8Array; review?: CrossNodeRecoveryReview; submitted?: boolean }) {
  let stage = input.stage, password = input.password, bytes: Uint8Array = input.bytes ?? new Uint8Array()
  let review = input.review ?? null, submitted = input.submitted ?? false, running = false, releaseRequested = false
  input.password = ''
  const check = () => { input.signal.throwIfAborted(); if (releaseRequested) throw new Error('原跨 Node 恢复流程已关闭。') }
  const seal = (material: string) => { const cleartext = encoder.encode(material); try { return recoverySeal(cleartext, password) } finally { cleartext.fill(0) } }
  const dispose = () => {
    input.signal.removeEventListener('abort', release)
    if ('root' in stage) { stage.root.free(); stage.opened.release() }
    if ('importer' in stage) stage.importer.free()
    if (stage.kind === 'finishing') stage.context.free()
    if (stage.kind === 'possession') stage.prepared.free()
    if (stage.kind === 'sealing' || stage.kind === 'prepared') stage.commit.free()
    if (stage.kind === 'committed') stage.verified.free()
    if (stage.kind === 'installing' || stage.kind === 'install_unknown' || stage.kind === 'installed') stage.finalBytes.fill(0)
    stage = { kind: 'released' }; closeEndpoints(input.endpoints); bytes.fill(0); password = ''
  }
  function release() { releaseRequested = true; closeEndpoints(input.endpoints); if (!running) dispose() }
  async function restoreLiveTargetRead(original: Extract<Stage, { kind: 'page' }>) {
    const target = await pinnedChannel(input.endpoints.target, input.signal)
    check()
    let progress = record(await target.request('identity', { method: 'cross_node_recovery_import_begin', params: { request: JSON.parse(original.requestJson), snapshot: original.snapshot } }))
    check()
    const saved = counter(progress.saved_page_count)
    if (!same(readBinding(progress), original.binding) || progress.page_count !== 0 || progress.complete !== false || saved > original.pageCount || saved + 1 < original.pageCount) throw new Error('目标 Node 的原恢复缓存进度与当前页不一致。')
    let count = 0
    while (count < saved) {
      progress = record(await target.request('identity', { method: 'cross_node_recovery_import_cached_page', params: original.binding }))
      check()
      const next = integer(progress.page_count)
      if (progress.state !== 'reading' || next !== count + 1 || progress.saved_page_count !== saved) throw new Error('目标 Node 没有逐页重建原恢复缓存。')
      count = next
    }
    return target
  }
  async function restoreTargetRead(checkpoint: Checkpoint, committed?: Record<string, unknown>) {
    const target = await pinnedChannel(input.endpoints.target, input.signal)
    check()
    const method = committed ? 'cross_node_recovery_import_resume' : 'cross_node_recovery_import_begin'
    let progress = record(await target.request('identity', { method, params: { request: checkpoint.request, snapshot: checkpoint.snapshot, ...(committed ? { committed } : {}) } }))
    check()
    let count = counter(progress.page_count)
    const binding = readBinding(checkpoint.context), saved = committed ? null : counter(progress.saved_page_count)
    if (!committed && !same(readBinding(progress), binding)) throw new Error('目标 Node 的恢复缓存换成了其他快照。')
    while (progress.complete !== true) {
      if (saved !== null && count >= saved) throw new Error('目标 Node 缺少原恢复上下文的完整缓存页。')
      progress = record(await target.request('identity', { method: 'cross_node_recovery_import_cached_page', params: committed ? { context: checkpoint.context } : binding }))
      check()
      const next = integer(progress.page_count)
      if (next !== count + 1 || saved !== null && progress.saved_page_count !== saved) throw new Error('目标 Node 的恢复缓存分页没有逐页推进。')
      count = next
    }
    const finished = record(await target.request('identity', { method: 'cross_node_recovery_import_finish', params: { context: checkpoint.context, witness: checkpoint.witness } }))
    check()
    if (finished.state !== 'ready' || !same(finished.context, checkpoint.context)) throw new Error('目标 Node 没有确认原有限恢复上下文。')
    return finished
  }
  input.signal.addEventListener('abort', release, { once: true })
  return {
    accountId: input.accountId, sourceEntry: input.endpoints.source.entry, targetEntry: input.endpoints.target.entry,
    sourceNodePublicKey: input.endpoints.source.publicKey, targetNodePublicKey: input.endpoints.target.publicKey,
    get phase() { return stage.kind }, get bytes() { check(); if (!bytes.length) throw new Error('请先完成原请求核对，再下载待提交恢复包。'); return bytes },
    get publicReview() { return review }, get submitted() { return submitted },
    get committed() { return stage.kind === 'committed' || stage.kind === 'installing' || stage.kind === 'install_unknown' || stage.kind === 'installed' },
    release,
    async prepare() {
      check(); if (running) throw new Error('原跨 Node 恢复请求仍在等待结果。')
      running = true
      try {
        if (stage.kind === 'request') {
          const original = stage, source = await pinnedChannel(input.endpoints.source, input.signal)
          check()
          const response = record(await source.request('identity', { method: 'cross_node_recovery_context', params: { request: JSON.parse(original.requestJson) } }))
          check()
          const snapshot = record(response.snapshot)
          let importer: Importer | null = beginCrossNodeRecoveryContextImport(original.opened.material, original.requestJson, JSON.stringify(snapshot), Date.now())
          try {
            const target = await pinnedChannel(input.endpoints.target, input.signal)
            check()
            const begun = record(await target.request('identity', { method: 'cross_node_recovery_import_begin', params: { request: JSON.parse(original.requestJson), snapshot } }))
            check()
            const binding = readBinding(begun), payload = record(snapshot.payload)
            if (begun.state !== 'reading' || begun.page_count !== 0 || begun.complete !== false || binding.snapshot_id !== payload.snapshot_id || binding.request_hash !== payload.request_hash) throw new Error('目标 Node 的有限读取绑定与原快照不一致。')
            stage = { ...original, kind: 'reading', importer, snapshot, binding, cursor: null }; importer = null
          } finally { importer?.free() }
        }
        while (stage.kind === 'reading' || stage.kind === 'page') {
          if (stage.kind === 'reading') {
            const original: Extract<Stage, { kind: 'reading' }> = stage
            const source = await pinnedChannel(input.endpoints.source, input.signal)
            check()
            const response = record(await source.request('identity', { method: 'cross_node_recovery_context_page', params: { snapshot_id: original.binding.snapshot_id, request_hash: original.binding.request_hash, cursor: original.cursor } }))
            check()
            const page = record(response.page), progress = record(JSON.parse(original.importer.appendPage(JSON.stringify(page), Date.now())))
            const complete = progress.complete === true, nextCursor = complete ? null : typedId(progress.cursor, 'h2_')
            if (!complete && nextCursor === original.cursor) throw new Error('跨 Node 恢复政策分页没有推进。')
            stage = { ...original, kind: 'page', page, nextCursor, complete, pageCount: integer(progress.page_count) }
          }
          if (stage.kind === 'page') {
            const original: Extract<Stage, { kind: 'page' }> = stage
            let target = await pinnedChannel(input.endpoints.target, input.signal), progress: Record<string, unknown>
            check()
            try { progress = record(await target.request('identity', { method: 'cross_node_recovery_import_page', params: { ...original.binding, page: original.page } })) }
            catch {
              check(); target = await restoreLiveTargetRead(original)
              progress = record(await target.request('identity', { method: 'cross_node_recovery_import_page', params: { ...original.binding, page: original.page } }))
            }
            check()
            if (progress.state !== 'reading' || progress.page_count !== original.pageCount || progress.complete !== original.complete) throw new Error('目标 Node 的有限恢复分页进度与客户端不一致。')
            const { page: _page, nextCursor, complete, pageCount: _pageCount, ...reading } = original
            stage = complete ? { ...reading, kind: 'witness' } : { ...reading, kind: 'reading', cursor: nextCursor }
          }
        }
        if (stage.kind === 'witness') {
          const original = stage, source = await pinnedChannel(input.endpoints.source, input.signal)
          check()
          const response = record(await source.request('identity', { method: 'cross_node_recovery_context_witness', params: { snapshot_id: original.binding.snapshot_id, request_hash: original.binding.request_hash, cursor: null } }))
          check()
          const witness = record(response.witness), witnessJson = JSON.stringify(witness)
          stage = { kind: 'request', root: original.root, opened: original.opened, requestJson: original.requestJson, certificateExpiresAt: original.certificateExpiresAt }
          let context: Context | null = null
          try {
            context = original.importer.finish(witnessJson, Date.now())
            const reference = record(record(JSON.parse(context.contextJson())).reference)
            if (!same(readBinding(reference), original.binding)) throw new Error('客户端完成的有限恢复引用与原目标分页绑定不一致。')
            const checkpoint = { request: record(JSON.parse(original.requestJson)), snapshot: original.snapshot, witness, context: reference }
            stage = { kind: 'finishing', root: original.root, opened: original.opened, context, checkpoint, certificateExpiresAt: original.certificateExpiresAt }; context = null
          } catch (error) { releaseRequested = true; throw error }
          finally { context?.free() }
        }
        if (stage.kind === 'finishing') {
          const original = stage, target = await pinnedChannel(input.endpoints.target, input.signal)
          check()
          let finished: Record<string, unknown>
          try { finished = record(await target.request('identity', { method: 'cross_node_recovery_import_finish', params: { context: original.checkpoint.context, witness: original.checkpoint.witness } })) }
          catch { check(); finished = await restoreTargetRead(original.checkpoint) }
          check()
          if (finished.state !== 'ready' || !same(finished.context, original.checkpoint.context)) throw new Error('目标 Node 没有完成原有限恢复上下文。')
          const prepared = prepareClientCrossNodeAccountRecovery(original.opened.material, original.context, original.root, original.certificateExpiresAt)
          stage = { kind: 'possession', root: original.root, opened: original.opened, prepared, request: record(JSON.parse(prepared.possessionRequestJson())), targetProof: null, sourceProof: null, checkpoint: original.checkpoint }
          original.context.free()
        }
        if (stage.kind === 'possession') {
          const original = stage
          if (!original.targetProof) {
            let target = await pinnedChannel(input.endpoints.target, input.signal)
            check()
            let response: Record<string, unknown>
            try { response = record(await target.request('identity', { method: 'cross_node_recovery_possession', params: original.request })) }
            catch {
              check(); await restoreTargetRead(original.checkpoint)
              target = await pinnedChannel(input.endpoints.target, input.signal)
              response = record(await target.request('identity', { method: 'cross_node_recovery_possession', params: original.request }))
            }
            check(); original.targetProof = text(response.proof)
          }
          if (!original.sourceProof) {
            const source = await pinnedChannel(input.endpoints.source, input.signal)
            check()
            const response = record(await source.request('identity', { method: 'cross_node_recovery_source_possession', params: original.request }))
            check(); original.sourceProof = text(response.proof)
          }
          const commit = finishCrossNodeAccountRecovery(original.prepared, original.root, original.targetProof, original.sourceProof, Date.now())
          stage = { kind: 'sealing', root: original.root, opened: original.opened, commit, checkpoint: original.checkpoint }; original.prepared.free()
        }
        if (stage.kind === 'sealing') {
          const original = stage
          review = publicReview(original.commit)
          if (review.accountId !== input.accountId || review.sourceRead.nodePublicKey !== input.endpoints.source.publicKey || review.targetNodePublicKey !== input.endpoints.target.publicKey) throw new Error('已验证恢复范围与原账户和节点不一致。')
          checkCheckpoint(original.checkpoint, original.commit)
          const pendingBytes = seal(pendingEnvelope(original.commit.recoveryMaterialJson(original.root, original.opened.material, input.endpoints.target.entry.address, Date.now()), original.checkpoint))
          stage = { kind: 'prepared', commit: original.commit, request: record(JSON.parse(original.commit.requestJson())), checkpoint: original.checkpoint }
          bytes.fill(0); bytes = pendingBytes; original.root.free(); original.opened.release()
        }
        return review
      } finally { running = false; if (releaseRequested) dispose() }
    },
    async commit(saved: boolean, persist: (result: CrossNodeRecoveryResult) => Promise<void>) {
      check(); if (running) throw new Error('原跨 Node 恢复请求仍在等待结果。')
      if (stage.kind !== 'prepared' && stage.kind !== 'committed' && stage.kind !== 'installing' && stage.kind !== 'install_unknown' && stage.kind !== 'installed') throw new Error('请先核对原请求并保存待提交恢复包。')
      if (stage.kind === 'prepared' && !saved) throw new Error('请先下载待提交恢复包，并确认已妥善保存。')
      if (stage.kind === 'installed') return stage.result
      running = true
      try {
        if (stage.kind === 'prepared') {
          const original = stage, source = await pinnedChannel(input.endpoints.source, input.signal)
          check()
          let receipt: Record<string, unknown> | null = null
          if (submitted) {
            const resume = record(JSON.parse(original.commit.resumeContextJson()))
            const status = record(await source.request('identity', { method: 'cross_node_recovery_status', params: { context: resume.context, input_hash: resume.input_hash } }))
            check()
            if (status.state === 'committed') receipt = record(status.receipt)
            else if (status.state !== 'not_committed') throw new Error('原跨 Node 恢复提交状态无法确认。请保留待提交恢复包继续对账。')
          }
          if (!receipt) {
            submitted = true
            const reply = record(await source.request('identity', { method: 'cross_node_recovery_commit', params: original.request }))
            check(); receipt = record(reply.receipt)
          }
          const verified = finishCrossNodeRecoveryCommit(original.commit, JSON.stringify(receipt), Date.now())
          stage = { kind: 'committed', verified, checkpoint: original.checkpoint }; original.commit.free()
        }
        if (stage.kind === 'committed') {
          if (!review) throw new Error('缺少原恢复请求的公开确认范围。')
          const original = stage, result = recoveryResult(original.verified, review, input.endpoints.source.publicKey)
          await persist(result)
          check()
          const nextBytes = seal(original.verified.finalRecoveryMaterialJson(input.endpoints.target.entry.address, Date.now()))
          const installation = record(JSON.parse(original.verified.installationJson()))
          stage = { kind: 'installing', result, installation, finalBytes: nextBytes, checkpoint: original.checkpoint }; password = ''; original.verified.free()
        } else if (stage.kind === 'installing' || stage.kind === 'install_unknown') {
          await persist(stage.result); check()
        }
        if (stage.kind !== 'installing' && stage.kind !== 'install_unknown') throw new Error('原恢复安装回执不可用。')
        const original = stage
        let target = await pinnedChannel(input.endpoints.target, input.signal), installed: Record<string, unknown> | null = null
        check()
        if (original.kind === 'install_unknown') {
          const status = record(await target.request('identity', { method: 'cross_node_recovery_install_status', params: original.installation }))
          check()
          if (status.state === 'installed') installed = record(status.installed)
          else if (status.state === 'not_installed') {
            await restoreTargetRead(original.checkpoint, original.installation)
            target = await pinnedChannel(input.endpoints.target, input.signal)
          } else throw new Error('新 Node 的原恢复安装状态无法确认，请保留同一请求继续对账。')
        }
        if (!installed) {
          stage = { ...original, kind: 'install_unknown' }
          installed = record(await target.request('identity', { method: 'cross_node_recovery_install', params: original.installation }))
        }
        check()
        if (installed.installed !== true || installed.account_id !== input.accountId || installed.node_public_key !== input.endpoints.target.publicKey || !same(installed.pins, original.result.trust.pins)) throw new Error('新 Node 尚未确认原恢复政策。请继续核对原请求，不能重新恢复。')
        bytes.fill(0); bytes = original.finalBytes
        stage = { ...original, kind: 'installed' }
        return original.result
      } finally { running = false; if (releaseRequested) dispose() }
    },
  }
}

export async function prepareCrossNodeRecovery(input: {
  file: File; password: string; newPassword: string; card: Extract<ConnectionCard, { kind: 'claim' }>; sourceAddress?: string
  sourceReadProjectIds: string[]; sourceReadExpiresAt: number; certificateExpiresAt: number; signal: AbortSignal
}) {
  let opened: Awaited<ReturnType<typeof openAccountPackage>> | null = null, root: Identity | null = null
  let claim: ReturnType<typeof verifyNodeClaim> | null = null, endpoints: Endpoints | null = null, transferred = false
  const close = () => { if (endpoints) closeEndpoints(endpoints) }
  input.signal.addEventListener('abort', close, { once: true })
  try {
    input.signal.throwIfAborted()
    if (input.card.entry.relay) throw new Error('请使用目标 Node 本机提供的直接认领卡。')
    opened = await openAccountPackage(input.file, input.password); input.password = ''; input.signal.throwIfAborted()
    if (opened.entry.nodePublicKey === input.card.nodePublicKey) throw new Error('跨 Node 恢复需要另一台独立 Node。')
    if (!input.sourceReadProjectIds.length || new Set(input.sourceReadProjectIds).size !== input.sourceReadProjectIds.length) throw new Error('请明确填写原 Node 的项目标识，每个项目只填写一次。')
    const sources = record(JSON.parse(inspectCrossNodeRecoverySources(opened.material, Date.now())))
    const knownProjects = array(sources.source_projects).map(resourceRef)
    const sourceReadResources = input.sourceReadProjectIds.map(id => {
      const matches = knownProjects.filter(project => project.resource === id)
      if (matches.length !== 1) throw new Error('所选项目不在恢复包已验证的原 Node 项目列表中。')
      return matches[0]
    })
    endpoints = { source: endpoint(opened.entry.nodePublicKey, input.sourceAddress ?? opened.entry.endpoint), target: { publicKey: input.card.nodePublicKey, entry: input.card.entry, channel: null } }
    const target = await pinnedChannel(endpoints.target, input.signal)
    const response = record(await target.request('identity', { method: 'node_claim', params: { capability: input.card.capability } }))
    input.signal.throwIfAborted()
    claim = verifyNodeClaim(JSON.stringify(record(response.claim)), input.card.nodePublicKey, input.card.claimNonce, Date.now())
    root = Identity.generate()
    const requestJson = buildClientCrossNodeRecoveryRequest(opened.material, root, claim, JSON.stringify(sourceReadResources), input.sourceReadExpiresAt, Date.now() + 120_000)
    const flow = recoveryFlow({ accountId: opened.scope.account_id, endpoints, stage: { kind: 'request', root, opened, requestJson, certificateExpiresAt: input.certificateExpiresAt }, signal: input.signal, password: input.newPassword })
    transferred = true
    return flow
  } finally {
    input.signal.removeEventListener('abort', close); claim?.free()
    if (!transferred) { root?.free(); opened?.release(); close() }
    input.password = ''; input.newPassword = ''
  }
}

export async function restoreCrossNodeRecovery(input: { file: File; password: string; newPassword?: string; sourceAddress?: string; targetAddress?: string; signal: AbortSignal }) {
  let commit: ReturnType<typeof restoreClientCrossNodeRecovery> | null = null, cleartext: Uint8Array | null = null, pendingJson = ''
  try {
    input.signal.throwIfAborted()
    if (!input.file.size || input.file.size > 2 * 1024 * 1024 + 4096) throw new Error('选择不超过 2 MiB 加密载体上限的待提交跨 Node 恢复包。')
    const encrypted = new Uint8Array(await input.file.arrayBuffer()); input.signal.throwIfAborted()
    cleartext = recoveryOpen(encrypted, input.password); pendingJson = decoder.decode(cleartext); cleartext.fill(0); cleartext = null
    const envelope = record(JSON.parse(pendingJson))
    if (envelope.format !== 'vibeface-client-cross-node-recovery-pending' || envelope.version !== 2 || Object.keys(envelope).length !== 4) throw new Error('请选择本次有限恢复流程保存的待提交跨 Node 恢复包。')
    const checkpoint = parseCheckpoint(envelope.read)
    commit = restoreClientCrossNodeRecovery(text(envelope.pending)); input.signal.throwIfAborted()
    checkCheckpoint(checkpoint, commit)
    const context = record(JSON.parse(commit.resumeContextJson())), source = record(context.source), target = record(context.target), scope = parseIdentityScope(context.scope)
    const endpoints = { source: endpoint(text(source.node_public_key), input.sourceAddress ?? text(source.endpoint)), target: endpoint(text(target.node_public_key), input.targetAddress ?? text(target.endpoint)) }
    const review = publicReview(commit), password = input.newPassword ?? input.password
    if (review.accountId !== scope.account_id || review.sourceRead.nodePublicKey !== endpoints.source.publicKey || review.targetNodePublicKey !== endpoints.target.publicKey) throw new Error('待提交恢复包的公开入口与原请求不一致。')
    const payload = encoder.encode(pendingJson)
    let bytes: Uint8Array
    try { bytes = recoverySeal(payload, password) } finally { payload.fill(0) }
    const flow = recoveryFlow({ accountId: scope.account_id, endpoints, stage: { kind: 'prepared', commit, request: record(JSON.parse(commit.requestJson())), checkpoint }, signal: input.signal, password, bytes, review, submitted: true })
    commit = null
    return flow
  } finally { commit?.free(); cleartext?.fill(0); pendingJson = ''; input.password = ''; input.newPassword = '' }
}
