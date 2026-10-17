import { deriveAuthorizationBodyHash, nodeFingerprint, verifySignedEvent, verifyTaskBinding } from '../../generated/vibeface_secure.js'
import { record, same } from './node'
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
      || typeof source.auto_followup !== 'boolean' || source.authorization_id !== `task:${task.task_id}`
      || payload.action !== 'task_create' || payload.operation_id !== source.authorization_id
      || payload.body_hash !== deriveAuthorizationBodyHash(JSON.stringify(source))
      || !same(payload.resource, { space: connection.scope.space_id, resource: connection.project.id })) throw new Error('工单创建证明与所选会话、项目或目标不一致。')
    // shortcut: historical tasks without local attempts trust the pinned author; verify their full Admission chain when a historical policy verifier is available.
    if (attempts && !attempts.some(attempt => same(attempt.body, source) && same(attempt.invocation, invocation))) throw new Error('工单回执未对应任何已保存的本地签名请求，请保留待核对记录。')
    return { taskRef: verified.hash, contextHash: verified.envelopeContextHash(), authorityNode: nodeFingerprint(connection.audiencePublicKey) }
  } finally { verified.free() }
}

export function verifyExecutionBinding(connection: TaskScope, task: TaskRecord, request: Pick<OperationRequest, 'conversation_id' | 'task_id' | 'target_node' | 'session_id'>, open: { type: 'open'; executor: string; cwd: string }, proof: unknown) {
  if (request.task_id !== task.task_id) throw new Error('执行请求与所选工单不一致。')
  if (request.target_node !== nodeFingerprint(connection.audiencePublicKey)) throw new Error('执行目标与已验证的项目设备不同。请连接目标设备后再操作。')
  const creation = verifyTaskCreation(connection, request.conversation_id, task)
  const expected = { binding: {
    space_id: connection.scope.space_id, conversation_id: request.conversation_id, task_ref: creation.taskRef,
    task_id: task.task_id, authority_node: creation.authorityNode, generation: 1, target_node: request.target_node,
    session_id: request.session_id, open_spec: open, previous_binding_hash: null, previous_seal: null,
  } }
  return record(JSON.parse(verifyTaskBinding(JSON.stringify(proof), connection.audiencePublicKey, JSON.stringify(expected))))
}
