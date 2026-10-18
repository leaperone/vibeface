import { deriveAuthorizationBodyHash, nodeFingerprint, verifySignedEvent, verifyTaskBinding } from '../../generated/vibeface_secure.js'
import { record, same, text } from './node'
import type { AuthorizedRequest, ReadyConnection } from './node'
import type { OperationRequest, TaskRecord } from '../features/workspace/types'

type TaskScope = Pick<ReadyConnection, 'scope' | 'project' | 'audiencePublicKey'>

export function verifyTaskCreation(connection: TaskScope, conversationId: string, task: TaskRecord, attempts?: AuthorizedRequest[]) {
  if (task.authority_public_key !== connection.audiencePublicKey) throw new Error('工单签名设备与已验证的项目入口不同。')
  const verified = verifySignedEvent(JSON.stringify(task.task_creation), connection.scope.space_id, connection.project.id, connection.audiencePublicKey)
  try {
    const event = record(JSON.parse(verified.bodyJson()))
    const cause = record(event.cause), admission = record(cause.admission)
    const source = record(admission.source_body), invocation = record(admission.invocation), payload = record(invocation.payload)
    if (event.scope_id !== conversationId || event.author !== nodeFingerprint(connection.audiencePublicKey)
      || cause.kind !== 'invocation' || !same(event.body, { type: 'task_created', task_id: task.task_id, title: task.title, goal: task.goal, acceptance: task.acceptance })
      || source.method !== 'create_task' || source.space_id !== connection.scope.space_id || source.resource_id !== connection.project.id
      || source.conversation_id !== conversationId || source.task_id !== task.task_id || source.title !== task.title || source.goal !== task.goal || source.acceptance !== task.acceptance
      || typeof source.auto_followup !== 'boolean' || typeof source.authorization_id !== 'string' || !source.authorization_id
      || payload.action !== 'task_create' || payload.operation_id !== source.authorization_id
      || payload.body_hash !== deriveAuthorizationBodyHash(JSON.stringify(source))
      || !same(payload.resource, { space: connection.scope.space_id, resource: connection.project.id })) throw new Error('工单创建证明与所选会话、项目或目标不一致。')
    // shortcut: historical tasks without local attempts trust the pinned author; verify their full Admission chain when a historical policy verifier is available.
    if (attempts && !attempts.some(attempt => same(attempt.body, source) && same(attempt.invocation, invocation))) throw new Error('工单回执未对应任何已保存的本地签名请求，请保留待核对记录。')
    return { taskRef: verified.hash, authorityNode: nodeFingerprint(connection.audiencePublicKey) }
  } finally { verified.free() }
}

function bindingLineage(proof: unknown, previousTargetPublicKey?: string) {
  const binding = record(record(proof).binding), generation = binding.generation
  if (typeof generation !== 'number' || !Number.isSafeInteger(generation) || generation < 1) throw new Error('执行绑定代次无效。')
  if (generation === 1) {
    if (binding.previous_binding_hash !== null || binding.previous_seal !== null) throw new Error('首次执行绑定不能带旧封存回执。')
    return { generation, previous_binding_hash: null, previous_seal: null }
  }
  if (!previousTargetPublicKey) throw new Error('请先独立核对并保存原执行端公钥，再核验重绑。')
  const previousSeal = record(binding.previous_seal), seal = record(previousSeal.seal)
  if (seal.target_node !== nodeFingerprint(previousTargetPublicKey) || binding.previous_binding_hash !== seal.binding_hash) throw new Error('重绑的原执行端公钥或封存绑定哈希不同。')
  return { generation, previous_binding_hash: text(binding.previous_binding_hash), previous_seal: previousSeal }
}

export function verifyExecutionBinding(connection: TaskScope, task: TaskRecord, request: Pick<OperationRequest, 'conversation_id' | 'task_id' | 'target_node' | 'session_id'>, open: { type: 'open'; executor: string; cwd: string }, proof: unknown, verifiedExecutionPublicKey = connection.audiencePublicKey, previousTargetPublicKey?: string) {
  if (request.task_id !== task.task_id) throw new Error('执行请求与所选工单不一致。')
  if (request.target_node !== nodeFingerprint(verifiedExecutionPublicKey)) throw new Error('执行目标与独立核验的设备不同。请重新核对执行授权。')
  const creation = verifyTaskCreation(connection, request.conversation_id, task)
  const lineage = bindingLineage(proof, previousTargetPublicKey)
  if (lineage.generation > 1 && !same(task.binding, proof)) throw new Error('重绑证明与当前任务已保存的绑定不同。')
  const expected = { binding: {
    space_id: connection.scope.space_id, conversation_id: request.conversation_id, task_ref: creation.taskRef,
    task_id: task.task_id, authority_node: creation.authorityNode, ...lineage, target_node: request.target_node,
    session_id: request.session_id, open_spec: open,
  }, ...(lineage.generation > 1 ? { previous_target_public_key: previousTargetPublicKey } : {}) }
  return record(JSON.parse(verifyTaskBinding(JSON.stringify(proof), connection.audiencePublicKey, JSON.stringify(expected))))
}

export function verifyBoundExecutionSelection(connection: TaskScope, conversationId: string, task: TaskRecord, previousTargetPublicKey?: string) {
  const binding = record(record(task.binding).binding), open = record(binding.open_spec)
  const creation = verifyTaskCreation(connection, conversationId, task)
  const lineage = bindingLineage(task.binding, previousTargetPublicKey)
  if (open.type !== 'open') throw new Error('原任务绑定没有有效的打开范围。')
  const sessionId = text(binding.session_id), executorKind = text(open.executor)
  const expected = { binding: {
    space_id: connection.scope.space_id, conversation_id: conversationId, task_ref: creation.taskRef,
    task_id: task.task_id, authority_node: creation.authorityNode, ...lineage, target_node: text(binding.target_node),
    session_id: sessionId, open_spec: { type: 'open', executor: executorKind, cwd: text(open.cwd) },
  }, ...(lineage.generation > 1 ? { previous_target_public_key: previousTargetPublicKey } : {}) }
  verifyTaskBinding(JSON.stringify(task.binding), connection.audiencePublicKey, JSON.stringify(expected))
  return { sessionId, executorKind }
}
