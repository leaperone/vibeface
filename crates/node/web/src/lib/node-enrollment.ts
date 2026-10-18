import { connectDevice, prepareClientAccountNodeEnrollmentStatus } from '../../transport.js'
import type { DeviceChannel, prepareClientAccountNodeEnrollment } from '../../transport.js'
import { finishAccountNodeEnrollment, nodeFingerprint, prepareAccountNodeEnrollmentCommit, verifyAccountNodeEnrollmentStatus, verifyNodeClaim } from '../../generated/vibeface_secure.js'
import { openAccountPackage } from './account'
import { counter, typedId } from './authorization-trust'
import { parseInfo, record, same, text } from './node'
import type { ConnectionCard, ConnectionEntry } from './node'

type Prepared = ReturnType<typeof prepareClientAccountNodeEnrollment>
type Commit = ReturnType<typeof prepareAccountNodeEnrollmentCommit>
type Enrollment = ReturnType<typeof finishAccountNodeEnrollment>
type Endpoint = { entry: ConnectionEntry; publicKey: string; channel: DeviceChannel | null }
type State =
  | { kind: 'begin'; params: Record<string, unknown> }
  | { kind: 'target_begin'; params: Record<string, unknown>; snapshot: Record<string, unknown> }
  | { kind: 'reading'; context: string; pageRequest: Record<string, unknown> }
  | { kind: 'page'; context: string; page: Record<string, unknown>; complete: boolean }
  | { kind: 'witness'; context: string; pageRequest: Record<string, unknown> }
  | { kind: 'possession'; context: string; proof: string }
  | { kind: 'prepared' | 'unknown'; context: string; request: Record<string, unknown> }
  | { kind: 'committed' | 'install_unknown' | 'installed'; context: string; installation: Record<string, unknown>; pins: unknown }
  | { kind: 'released' }

export async function prepareNodeEnrollment(input: {
  file: File
  password: string
  accountId: string
  card: Extract<ConnectionCard, { kind: 'claim' }>
  certificateExpiresAt: number
  signal: AbortSignal
  prepare(material: string, claim: ReturnType<typeof verifyNodeClaim>, certificateExpiresAt: number): Promise<Prepared>
}) {
  let opened: Awaited<ReturnType<typeof openAccountPackage>> | null = null
  let claim: ReturnType<typeof verifyNodeClaim> | null = null, prepared: Prepared | null = null, commit: Commit | null = null
  let state: State = { kind: 'released' }, source: Endpoint | null = null
  const target: Endpoint = { entry: input.card.entry, publicKey: input.card.nodePublicKey, channel: null }
  let running = true, releaseRequested = false
  const close = () => { source?.channel?.close(); target.channel?.close() }
  const check = () => { input.signal.throwIfAborted(); if (releaseRequested) throw new Error('原 Node 入网流程已关闭。') }
  const dispose = () => {
    input.signal.removeEventListener('abort', release)
    opened?.release(); opened = null
    prepared?.free(); prepared = null
    claim?.free(); claim = null
    commit?.free(); commit = null
    close(); source = null; state = { kind: 'released' }; input.password = ''; input.card.capability = ''; input.card.claimNonce = ''
  }
  function release() { releaseRequested = true; close(); if (!running) dispose() }
  async function channel(endpoint: Endpoint) {
    check()
    if (endpoint.channel) return endpoint.channel
    let candidate: DeviceChannel | null = null
    candidate = await connectDevice(endpoint.entry, { signal: input.signal, onClose() { if (endpoint.channel === candidate) endpoint.channel = null } })
    if (candidate.publicKey !== endpoint.publicKey) { candidate.close(); throw new Error('Node 的加密连接公钥与独立保存的目标不同。') }
    endpoint.channel = candidate
    try {
      const info = parseInfo(await candidate.request('identity', { method: 'status', params: {} }))
      check()
      if (info.public_key !== endpoint.publicKey || info.node_id !== endpoint.entry.node_id) throw new Error('Node 报告的身份与固定连接目标不同。')
      return candidate
    } catch (error) { candidate.close(); if (endpoint.channel === candidate) endpoint.channel = null; throw error }
  }
  async function sourceCall(method: string, params: Record<string, unknown>) {
    if (!source) throw new Error('原账户授权 Node 的入口已释放。')
    const active = await channel(source)
    check()
    const result = record(await active.request('identity', { method, params }))
    check()
    return result
  }
  async function targetCall(method: string, params: Record<string, unknown>) {
    const active = await channel(target)
    check()
    const result = record(await active.request('identity', { method, params }))
    check()
    return result
  }
  async function resumeTarget(context: string, installation?: Record<string, unknown>) {
    let previous = 0
    for (;;) {
      const result = await targetCall('account_node_import_resume', { context, ...(installation ? { installation } : {}) })
      if (result.context !== context) throw new Error('新 Node 更换了原入网上下文。')
      if (result.ready === true || result.page_request !== undefined) return result
      const count = counter(result.replayed_pages), total = counter(result.cached_pages)
      if (count <= previous || count > total) throw new Error('新 Node 的原入网缓存没有逐页推进。')
      previous = count
    }
  }
  function accepted(verified: Enrollment, context: string) {
    try { return { kind: 'committed', context, installation: record(JSON.parse(verified.installationJson())), pins: JSON.parse(verified.pinsJson()) } satisfies State }
    finally { verified.free() }
  }
  input.signal.addEventListener('abort', release, { once: true })
  try {
    check()
    opened = await openAccountPackage(input.file, input.password)
    input.password = ''; check()
    if (opened.scope.account_id !== input.accountId) throw new Error('恢复包不属于当前账户。')
    if (input.card.entry.relay) throw new Error('请使用新 Node 本机提供的直接连接卡。')
    if (opened.entry.nodePublicKey === target.publicKey) throw new Error('请选择尚未加入账户的另一台 Node。')
    source = { publicKey: opened.entry.nodePublicKey, entry: { address: opened.entry.endpoint, node_id: nodeFingerprint(opened.entry.nodePublicKey), relay: false }, channel: null }
    const claimed = await targetCall('node_claim', { capability: input.card.capability })
    claim = verifyNodeClaim(JSON.stringify(record(claimed.claim)), target.publicKey, input.card.claimNonce, Date.now())
    prepared = await input.prepare(opened.material, claim, input.certificateExpiresAt)
    check()
    state = { kind: 'begin', params: record(JSON.parse(prepared.requestJson())) }
    claim.free(); claim = null; input.card.capability = ''; input.card.claimNonce = ''
    running = false
    return {
      accountId: input.accountId, nodePublicKey: target.publicKey, entry: target.entry,
      get phase() { return state.kind },
      release,
      async prepare() {
        check()
        if (running) throw new Error('原 Node 入网请求仍在等待结果。')
        running = true
        try {
          if (state.kind === 'begin') {
            const original = state
            const response = await sourceCall('enrollment_policy_read_begin', original.params)
            state = { kind: 'target_begin', params: original.params, snapshot: record(response.snapshot) }
          }
          if (state.kind === 'target_begin') {
            const original = state
            const response = await targetCall('account_node_import_begin', { ...original.params, snapshot: original.snapshot })
            state = { kind: 'reading', context: typedId(response.context, 'h2_'), pageRequest: record(response.page_request) }
          }
          while (state.kind === 'reading' || state.kind === 'page') {
            if (state.kind === 'reading') {
              const original: Extract<State, { kind: 'reading' }> = state
              const response = await sourceCall('enrollment_policy_read_page', original.pageRequest)
              const page = record(response.page)
              state = { kind: 'page', context: original.context, page, complete: record(page.payload).complete === true }
            }
            if (state.kind === 'page') {
              const original: Extract<State, { kind: 'page' }> = state
              let response: Record<string, unknown>
              try { response = await targetCall('account_node_import_page', { context: original.context, page: original.page }) }
              catch { check(); await resumeTarget(original.context); response = await targetCall('account_node_import_page', { context: original.context, page: original.page }) }
              if (response.context !== original.context) throw new Error('新 Node 返回了其他入网上下文。')
              const pageRequest = record(response.page_request)
              state = original.complete ? { kind: 'witness', context: original.context, pageRequest } : { kind: 'reading', context: original.context, pageRequest }
            }
          }
          if (state.kind === 'witness') {
            const original = state
            const response = await sourceCall('enrollment_policy_read_witness', { ...original.pageRequest, cursor: null })
            let possession: Record<string, unknown>
            try { possession = await targetCall('account_node_import_finish', { context: original.context, witness: response.witness }) }
            catch {
              check(); const resumed = await resumeTarget(original.context)
              possession = resumed.ready === true ? resumed : await targetCall('account_node_import_finish', { context: original.context, witness: response.witness })
            }
            if (possession.context !== original.context) throw new Error('新 Node 的持有证明属于其他入网上下文。')
            state = { kind: 'possession', context: original.context, proof: text(possession.proof) }
          }
          if (state.kind === 'possession') {
            if (!prepared || !opened) throw new Error('原入网私料已释放。')
            const original = state
            commit = prepareAccountNodeEnrollmentCommit(prepared, opened.material, original.proof, Date.now())
            state = { kind: 'prepared', context: original.context, request: record(JSON.parse(commit.requestJson())) }
            prepared.free(); prepared = null; opened.release(); opened = null
          }
        } finally { running = false; if (releaseRequested) dispose() }
      },
      async commit(persist: (pins: unknown) => Promise<void>) {
        check()
        if (running) throw new Error('原 Node 入网请求仍在等待结果。')
        if (state.kind !== 'prepared' && state.kind !== 'unknown' && state.kind !== 'committed' && state.kind !== 'install_unknown' && state.kind !== 'installed') throw new Error('请先完成原 Node 的有限政策导入和持有证明。')
        if (state.kind === 'installed') return
        running = true
        try {
          if (state.kind === 'prepared' || state.kind === 'unknown') {
            if (!commit) throw new Error('原 Node 入网提交对象已释放。')
            const original = state
            if (original.kind === 'unknown') {
              const statusRequest = prepareClientAccountNodeEnrollmentStatus(commit, Date.now() + 120_000)
              let status: ReturnType<typeof verifyAccountNodeEnrollmentStatus> | null = null
              try {
                const response = await sourceCall('account_node_status', record(JSON.parse(statusRequest.requestJson())))
                status = verifyAccountNodeEnrollmentStatus(statusRequest, JSON.stringify(response.status), Date.now())
                const current = status.state()
                if (current === 'committed') {
                  const verified = status.enrollment()
                  if (!verified) throw new Error('原入网状态没有提供已验证的完成结果。')
                  state = accepted(verified, original.context)
                } else if (current !== 'not_observed') throw new Error('原 Node 入网状态无法确认。')
              } finally { status?.free(); statusRequest.free() }
            }
            if (state.kind === 'prepared' || state.kind === 'unknown') {
              state = { ...original, kind: 'unknown' }
              const response = await sourceCall('account_node_commit', original.request)
              state = accepted(finishAccountNodeEnrollment(commit, JSON.stringify(response.journal_delta), Date.now()), original.context)
            }
          }
          if (state.kind !== 'committed' && state.kind !== 'install_unknown') throw new Error('原 Node 入网完成结果不可用。')
          const original = state
          await persist(original.pins)
          check()
          let installed: Record<string, unknown> | null = null
          const installation = { context: original.context, installation: original.installation }
          if (original.kind === 'install_unknown') {
            const status = await targetCall('account_node_install_status', installation)
            if (status.state === 'committed') installed = record(status.response)
            else if (status.state === 'not_observed') await resumeTarget(original.context, original.installation)
            else throw new Error('新 Node 的原安装状态无法确认，请保留同一请求继续对账。')
          }
          if (!installed) {
            state = { ...original, kind: 'install_unknown' }
            installed = await targetCall('account_node_install', installation)
          }
          if (installed.installed !== true || installed.account_id !== input.accountId || installed.node_public_key !== target.publicKey || !same(installed.pins, original.pins)) throw new Error('新 Node 尚未确认安装原入网政策，请保留同一请求继续核对。')
          state = { ...original, kind: 'installed' }
        } finally { running = false; if (releaseRequested) dispose() }
      },
    }
  } catch (error) { running = false; release(); throw error }
}
