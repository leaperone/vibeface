import * as secure from '../../generated/vibeface_secure.js'
import { parseAuthorizationTrust } from './authorization-trust.ts'
import type { AuthorizationTrust } from './authorization-trust.ts'
import { parseEntry } from './node.ts'
import type { ConnectionEntry } from './node.ts'
import { parseExecutionWork, parseExecutionWorkIntent } from './execution-work.ts'
import type { ExecutionWork, ExecutionWorkIntent } from './execution-work.ts'
function isRecord(value: unknown): value is Record<string, unknown> { return value !== null && typeof value === 'object' && !Array.isArray(value) }
function record(value: unknown, label = '账户授权数据'): Record<string, unknown> {
  if (!isRecord(value)) throw new Error(`${label}格式无效。`)
  return value
}
function text(value: unknown): string {
  if (typeof value !== 'string' || !value) throw new Error('账户授权文本缺失。')
  return value
}
function array(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error('账户授权列表格式无效。')
  return value
}

export type RemoteExecutionSelection = {
  execution_resource: { space: string; resource: string }
  target_public_key: string
  execution_grant: string
}
export type RemoteExecutionShare = {
  version: 2
  kind: 'vibeface-remote-execution'
  purpose: RemoteExecutionPurpose
  selection: RemoteExecutionSelection
  context: { conversation: string; task: string | null; sdk_session: null; execution_session: { target: string; session_id: string } | null; executor_kind: string | null; workspace_resource: string }
  work: ExecutionWorkIntent
  actions: string[]
  sourceProject: { space: string; resource: string }
}
export type TrustedRemoteExecution = { share: RemoteExecutionShare; trust: AuthorizationTrust }
export type RemoteExecutionSourceTrust = { trust: AuthorizationTrust; entry: ConnectionEntry; nodePublicKey: string }
export type RemoteExecutionPurpose = 'execution' | 'resolution' | 'rebind'
export type RemoteExecutionSession = {
  targetNode: string; sessionId: string; taskRef?: string; revision: number; bindingGeneration?: number; work: Exclude<ExecutionWork, { kind: 'command' }>
  executor: string; cwd: string; state: string; expiresAt: number; receipt: Record<string, unknown>
}
export const remoteExecutionShareActions = [
  'execution_open', 'execution_send', 'execution_steer', 'execution_interrupt', 'execution_respond_approval',
  'operation_status_read', 'operation_resolve', 'execution_resolve', 'execution_rebind',
  'execution_command',
] as const
export type RemoteExecutionShareAction = typeof remoteExecutionShareActions[number]
export type RemoteExecutionShareInput = {
  requestJson: string; executionResourceId: string; actions: RemoteExecutionShareAction[]; expiresAt: number
}
export type RemoteExecutionRequestSummary = {
  requestHash: string; projectResourceId: string; projectLabel: string; taskId: string; purpose: RemoteExecutionPurpose
  executorKind: string; servicePublicKey: string; expiresAt: number
}
export type RemoteExecutionRequestInput = {
  conversationId: string; taskId: string; taskRef: string; taskCreation: Record<string, unknown>
  sessionId: string; executorKind: string; expiresAt: number; purpose: RemoteExecutionPurpose
  executionResource?: RemoteExecutionSelection['execution_resource']; targetPublicKey?: string; oldBinding?: Record<string, unknown>; oldExecutionReceipt?: Record<string, unknown>
}
export type RemoteExecutionState = 'accepted' | 'running' | 'unknown' | 'succeeded' | 'failed' | 'rejected'
export type RemoteExecutionObservation = {
  actual: Record<string, unknown>; receipt: Record<string, unknown>; operationId: string; state: RemoteExecutionState; result: unknown
}
export type RemoteExecutionResolutionResult = {
  state: 'succeeded' | 'failed' | 'rejected'; purpose: 'resolve_unknown' | 'reconcile_terminal'; actual: Record<string, unknown>
}

function id(value: unknown, prefix: string) {
  const result = text(value)
  if (!new RegExp(`^${prefix}[a-f0-9]{64}$`).test(result)) throw new Error('远端执行授权标识格式无效。')
  return result
}

function isRemoteExecutionShareAction(value: unknown): value is RemoteExecutionShareAction {
  return remoteExecutionShareActions.some(action => action === value)
}

export function parseRemoteExecutionShareInput(value: unknown, now: number): RemoteExecutionShareInput {
  const input = record(value), actions = array(input.actions)
  if (!actions.length || !actions.every(isRemoteExecutionShareAction) || new Set(actions).size !== actions.length) throw new Error('远端授权只能选择不重复的受限操作。')
  const resolution = actions.includes('execution_resolve') || actions.includes('operation_resolve')
  const rebind = actions.includes('execution_rebind')
  if (rebind ? actions.length !== 2 || !actions.includes('operation_status_read')
    : resolution ? actions.length !== 2 || !actions.includes('execution_resolve') || !actions.includes('operation_resolve')
    : !actions.includes('execution_open') || !actions.includes('operation_status_read')) throw new Error('请分别生成执行、结果处理或 Rebind 授权。Rebind 只允许封存和读取封存状态，结果处理只允许执行端处理和项目裁定。')
  if (typeof input.expiresAt !== 'number' || !Number.isSafeInteger(input.expiresAt) || input.expiresAt <= now) throw new Error('请明确选择晚于当前时刻的授权有效期。')
  const requestJson = text(input.requestJson)
  if (new TextEncoder().encode(requestJson).length > 65_536) throw new Error('执行申请超过大小限制。')
  record(JSON.parse(requestJson), '受签执行申请')
  return {
    requestJson, executionResourceId: id(input.executionResourceId, 'res2_'), actions, expiresAt: input.expiresAt,
  }
}

export function parseRemoteExecutionShare(value: unknown): RemoteExecutionShare {
  const share = record(value), selection = record(share.selection), resource = record(selection.execution_resource)
  if (new TextEncoder().encode(JSON.stringify(share)).length > 65_536 || share.version !== 2 || share.kind !== 'vibeface-remote-execution'
    || Object.keys(share).some(key => !['version', 'kind', 'purpose', 'selection', 'context', 'actions', 'sourceProject', 'work'].includes(key))) throw new Error('需要当前格式的有限执行授权引用。')
  const purpose = share.purpose
  if (purpose !== 'execution' && purpose !== 'resolution' && purpose !== 'rebind') throw new Error('执行授权引用必须明确单一用途。')
  const context = record(share.context), work = parseExecutionWorkIntent(share.work), session = context.execution_session === null ? null : record(context.execution_session), project = record(share.sourceProject)
  const targetPublicKey = id(selection.target_public_key, ''), executionResource = { space: id(resource.space, 'spc2_'), resource: id(resource.resource, 'res2_') }
  const actions = array(share.actions).map(text)
  const rebind = actions.includes('execution_rebind'), resolution = actions.includes('execution_resolve') || actions.includes('operation_resolve')
  const invalidPurpose = purpose === 'rebind' ? !rebind || actions.length !== 2 || !actions.includes('operation_status_read')
    : purpose === 'resolution' ? !resolution || actions.length !== 2 || !actions.includes('execution_resolve') || !actions.includes('operation_resolve')
    : rebind || resolution || !actions.includes('execution_submit') || !actions.some(action => action !== 'execution_submit') || actions.some(action => action !== 'execution_submit' && !isRemoteExecutionShareAction(action))
  if (new Set(actions).size !== actions.length || invalidPurpose) throw new Error('执行授权引用混合了不同用途或包含未支持的操作。')
  if (context.sdk_session !== null || session !== null && session.target !== secure.deriveAuthorizationId('device', targetPublicKey) || context.workspace_resource !== executionResource.resource) throw new Error('执行授权引用的上下文与目标资源不同。')
  if (work.kind === 'command') {
    if (context.task !== null || session !== null || context.executor_kind !== null || purpose === 'rebind'
      || purpose === 'execution' && (!actions.includes('execution_command') || actions.some(action => !['execution_command', 'execution_submit', 'operation_status_read'].includes(action)))) throw new Error('命令授权不能包含任务、会话或会话控制。')
  } else if (!session || work.execution_session.session_id !== session.session_id || work.execution_session.target !== session.target
    || work.kind === 'task_session' && work.task !== context.task
    || work.kind === 'conversation_session' && (context.task !== null || work.conversation !== context.conversation || work.executor !== context.executor_kind)) throw new Error('执行授权与工作上下文不同。')
  return {
    version: 2, kind: 'vibeface-remote-execution', purpose,
    selection: { execution_resource: executionResource, target_public_key: targetPublicKey, execution_grant: id(selection.execution_grant, 'h2_') },
    context: { conversation: text(context.conversation), task: context.task === null ? null : id(context.task, ''), sdk_session: null,
      execution_session: session === null ? null : { target: id(session.target, 'dev2_'), session_id: text(session.session_id) }, executor_kind: context.executor_kind === null ? null : text(context.executor_kind), workspace_resource: executionResource.resource }, work,
    actions, sourceProject: { space: id(project.space, 'spc2_'), resource: id(project.resource, 'res2_') },
  }
}

export function parseTrustedRemoteExecution(value: unknown): TrustedRemoteExecution {
  const input = record(value), share = parseRemoteExecutionShare(input.share), trust = parseAuthorizationTrust(input.trust)
  if (trust.spaceId !== share.selection.execution_resource.space
    || !trust.pins.some(pin => pin.domain.kind === 'account' && pin.domain.id === trust.accountId)
    || !trust.pins.some(pin => pin.domain.kind === 'space' && pin.domain.id === trust.spaceId)) throw new Error('独立保存的根信任未覆盖远端执行账户和空间。')
  return { share, trust }
}

export function remoteExecutionProposal(remote: TrustedRemoteExecution) {
  const reference = parseRemoteExecutionShare(remote.share), context = reference.context
  return { conversationId: context.conversation, taskRef: context.task, sessionId: context.execution_session?.session_id ?? null,
    targetDevice: secure.deriveAuthorizationId('device', reference.selection.target_public_key), executorKind: context.executor_kind, purpose: reference.purpose, work: reference.work }
}

export function taskRemoteExecutionProposal(remote: TrustedRemoteExecution) {
  const proposal = remoteExecutionProposal(remote)
  if (proposal.work.kind !== 'task_session') throw new Error('此入口需要任务执行授权。')
  return { ...proposal, taskRef: text(proposal.taskRef), sessionId: text(proposal.sessionId), executorKind: text(proposal.executorKind) }
}

export function parseRemoteExecutionSourceTrust(value: unknown): RemoteExecutionSourceTrust {
  const card = record(value), entry = parseEntry(card.entry), nodePublicKey = id(card.nodePublicKey, '')
  if (card.version !== 2 || card.kind !== 'vibeface-source-trust' || entry.node_id !== secure.nodeFingerprint(nodePublicKey)
    || Object.keys(card).some(key => !['version', 'kind', 'trust', 'entry', 'nodePublicKey'].includes(key))
    || new TextEncoder().encode(JSON.stringify(card)).length > 65_536) throw new Error('需要包含独立 Node 公钥及入口的当前格式 B 来源信任卡。')
  return { trust: parseAuthorizationTrust(card.trust), entry, nodePublicKey }
}

export function remoteExecutionParams(remote: TrustedRemoteExecution, receipt?: Record<string, unknown>) {
  return { ...remote.share.selection, ...(receipt ? { execution_session_receipt: receipt } : {}) }
}

export function parseRemoteExecutionSession(value: unknown): RemoteExecutionSession {
  const snapshot = record(value)
  const counter = (value: unknown) => {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new Error('远端执行快照版本无效。')
    return value
  }
  const work = parseExecutionWork(snapshot.work)
  if (work.kind === 'command') throw new Error('命令不能产生执行会话。')
  const executionSession = work.execution_session
  if (executionSession.session_id !== snapshot.session_id) throw new Error('执行快照与工作会话不同。')
  return {
    targetNode: id(snapshot.target_node, ''), sessionId: text(snapshot.session_id), ...(work.kind === 'task_session' ? { taskRef: id(work.task, ''), bindingGeneration: counter(work.binding_generation) } : {}), work,
    revision: counter(snapshot.revision), executor: text(snapshot.executor),
    cwd: text(snapshot.cwd), state: text(snapshot.state), expiresAt: counter(snapshot.expires_at_ms), receipt: record(snapshot.execution_session_receipt),
  }
}

export function parseRemoteExecutionObservation(value: unknown): RemoteExecutionObservation {
  const actual = record(value), receipt = record(actual.receipt), payload = record(receipt.payload)
  const state = payload.state
  if (state !== 'accepted' && state !== 'running' && state !== 'unknown' && state !== 'succeeded' && state !== 'failed' && state !== 'rejected') throw new Error('远端执行回执包含不支持的操作状态。')
  return { actual, receipt, operationId: text(payload.operation_id), state, result: actual.result }
}

export function parseRemoteExecutionResolutionResult(value: unknown): RemoteExecutionResolutionResult {
  const result = record(value)
  if (result.state !== 'succeeded' && result.state !== 'failed' && result.state !== 'rejected'
    || result.purpose !== 'resolve_unknown' && result.purpose !== 'reconcile_terminal') throw new Error('远端裁定回执状态或用途无效。')
  return { state: result.state, purpose: result.purpose, actual: record(result.actual) }
}

export type RemoteExecutionControl = { turn_id: string | null; pending_requests: { request_id: string | number; turn_id: string }[]; expires_at_ms: number }
export type TaskExecutionWorkIntent = { kind: 'task_session'; task: string; binding_generation: number; execution_session: { target: string; session_id: string } }
export function parseRemoteExecutionControl(value: unknown): RemoteExecutionControl {
  const control = record(value), expiresAt = control.expires_at_ms
  if (typeof expiresAt !== 'number' || !Number.isSafeInteger(expiresAt) || expiresAt < 1) throw new Error('执行控制回执的有效期无效。')
  const turnId = control.turn_id === null ? null : text(control.turn_id)
  const requests = array(control.pending_requests).map(value => {
    const request = record(value), requestId = request.request_id
    if (typeof requestId !== 'string' && (typeof requestId !== 'number' || !Number.isSafeInteger(requestId))) throw new Error('执行控制回执包含无效请求编号。')
    const requestTurn = text(request.turn_id)
    if (turnId === null || requestTurn !== turnId) throw new Error('执行审批不属于当前已核验轮次。')
    return { request_id: requestId, turn_id: requestTurn }
  })
  return { turn_id: turnId, pending_requests: requests, expires_at_ms: expiresAt }
}
