import { array, record, same, text } from './node.ts'
import type { RpcFacts } from './node.ts'

export function prepareContext(body: Record<string, unknown>, audienceDeviceId: string, resourceId: string, facts?: RpcFacts, projectMetadata?: Record<string, unknown>, pinnedAudiencePublicKey?: string) {
  if (body.method === 'seal_remote_execution' || body.method === 'remote_execution_seal_status') {
    const receipt = record(body.execution_receipt), payload = record(receipt.payload), session = record(payload.session)
    const binding = record(record(body.binding).binding), resource = record(session.execution_resource), originalProject = record(session.original_project), open = record(binding.open_spec)
    const generation = session.binding_generation
    if (!pinnedAudiencePublicKey || receipt.issuer !== pinnedAudiencePublicKey || payload.target !== audienceDeviceId || session.target !== audienceDeviceId
      || body.resource_id !== resourceId || resource.resource !== resourceId || body.space_id !== resource.space || payload.execution_space_id !== resource.space
      || body.original_space_id !== payload.original_space_id || body.original_space_id !== binding.space_id || body.original_space_id !== originalProject.space
      || body.operation_id !== payload.operation_id || payload.session_id !== session.session_id || binding.session_id !== session.session_id
      || open.type !== 'open' || open.executor !== session.executor || open.cwd !== session.cwd
      || binding.task_ref !== session.task || facts?.taskRef !== undefined && facts.taskRef !== session.task
      || typeof generation !== 'number' || !Number.isSafeInteger(generation) || generation < 1 || binding.generation !== generation || payload.binding_generation !== generation
      || ['actual', 'execution_result', 'result'].some(key => key in body)) throw new Error('封存请求必须使用原执行回执、绑定和独立核验的执行端公钥。')
    return {
      context: { conversation: text(binding.conversation_id), task: text(session.task), sdk_session: null,
        execution_session: { target: audienceDeviceId, session_id: text(session.session_id) }, executor_kind: text(session.executor), workspace_resource: resourceId },
      binding_generation: generation, canonical_cwd: null, execution_target_public_key: pinnedAudiencePublicKey,
    }
  }
  const request = body.request === undefined ? null : record(body.request)
  const conversation = body.conversation_id ?? request?.conversation_id ?? null
  const taskId = body.task_id ?? request?.task_id
  if (taskId != null && body.method !== 'create_task' && !facts?.taskRef) throw new Error('此请求缺少本地已验证的工单创建证明。')
  if (facts?.taskRef !== undefined && !/^[a-f0-9]{64}$/.test(facts.taskRef)) throw new Error('工单事件引用无效。')
  if (facts?.projectWork) {
    if (!projectMetadata) throw new Error('原操作缺少已核验的项目派单。')
    const context = record(projectMetadata.context)
    if (context.conversation !== conversation || context.workspace_resource !== resourceId) throw new Error('项目派单属于其他会话或项目。')
    return projectMetadata
  }
  if (facts?.projectExecutionMetadata) {
    if (!projectMetadata || !same(projectMetadata.context, { conversation, task: facts.taskRef ?? null, sdk_session: null, execution_session: null, executor_kind: null, workspace_resource: resourceId })) throw new Error('项目执行历史证明与本地会话或工单不同。')
    return projectMetadata
  }
  const execution = facts?.execution
  if ((body.session_id != null || request?.session_id != null) && !execution) throw new Error('此请求缺少本地已验证的执行会话。')
  return {
    context: {
      conversation: conversation === null ? null : text(conversation), task: facts?.taskRef ?? null, sdk_session: null,
      execution_session: execution ? { target: facts?.remote?.targetDevice ?? audienceDeviceId, session_id: execution.sessionId } : null,
      executor_kind: execution?.executorKind ?? null, workspace_resource: resourceId,
    },
    binding_generation: execution?.bindingGeneration ?? null, canonical_cwd: execution?.canonicalCwd ?? null,
    ...(facts?.remote ? { execution_target_public_key: facts.remote.authorization.share.selection.target_public_key } : {}),
  }
}

export function verifyPreparedBody(raw: Record<string, unknown>, normalized: Record<string, unknown>, facts?: RpcFacts, control?: unknown) {
  if (raw.method === 'execution_control' || raw.method === 'task_control' && raw.action !== 'status') {
    const proof = facts?.control
    if (!proof || facts?.remote || proof.originalRequest.target_node === undefined) throw new Error('此控制缺少本机已核验的原操作及实时状态。')
    const original = proof.originalRequest
    if (original.operation_id !== raw.expected_operation_id || original.conversation_id !== raw.conversation_id
      || original.space_id !== raw.space_id || !same(raw.basis_heads, proof.basisHeads)) throw new Error('控制请求与本地原操作或会话不同。')
    const action = raw.method === 'task_control' ? { type: raw.action, text: raw.text, request_id: raw.request_id, response: raw.response } : record(raw.action)
    let finalAction: Record<string, unknown>
    if (action.type === 'interrupt') finalAction = { type: 'interrupt', turn_id: proof.turnId }
    else if (action.type === 'steer' && typeof action.text === 'string') finalAction = { type: 'steer', text: action.text, turn_id: proof.turnId }
    else if (action.type === 'respond_request') {
      if (!proof.pendingRequests.some(request => request.id === action.request_id && request.threadId === proof.nativeSessionId && request.turnId === proof.turnId)) throw new Error('审批请求不属于已核验的当前执行轮次。')
      finalAction = { type: 'respond_request', request_id: action.request_id, response: record(action.response) }
    } else throw new Error('不支持此执行控制。')
    if (control !== undefined) {
      const evidence = record(control)
      if (!same(evidence.intent, raw) || !same(evidence.original_request, original)) throw new Error('节点展开控制时改变了原意图或原操作。')
      const status = record(evidence.status), session = record(status.session), snapshot = record(status.snapshot)
      const turn = record(record(status.turn).turn), current = record(record(record(snapshot.threads)[proof.nativeSessionId]).last_turn)
      if (session.native_session_id !== proof.nativeSessionId || session.session_id !== original.session_id || session.target_node !== original.target_node
        || session.revision !== raw.expected_revision || turn.id !== proof.turnId || current.id !== proof.turnId
        || !['inProgress', 'in_progress', 'running'].includes(text(current.status))) throw new Error('节点展开控制时使用了其他会话、版本或执行轮次。')
    }
    const { manager_authority: _managerAuthority, ...request } = original
    const expected = { method: 'operate', space_id: raw.space_id, resource_id: raw.resource_id, authorization_id: raw.operation_id,
      request: { ...request, operation_id: raw.operation_id, expected_revision: raw.expected_revision, basis_heads: proof.basisHeads,
        action: finalAction, resources: [] } }
    if (!same(normalized, expected)) throw new Error('节点展开的最终执行请求与本人意图不同。')
    return
  }
  const expected = record(JSON.parse(JSON.stringify(raw)))
  if (raw.method === 'update_task' && raw.detail === undefined) expected.detail = ''
  if (raw.method === 'operate') {
    const request = record(expected.request), finalRequest = record(normalized.request), action = record(request.action)
    if ((action.type === 'open' || action.type === 'command') && record(finalRequest.action).cwd !== action.cwd) throw new Error('节点报告了不同的实际工作目录。请核对规范路径后重新提交。')
    const resources = array(request.resources).map(value => text(value))
    if (action.type === 'open' || action.type === 'send') {
      const cwd = facts?.execution?.canonicalCwd
      if (!cwd) throw new Error('无法独立确认执行资源的工作目录。')
      resources.push(cwd)
    }
    if (action.type === 'command') resources.push(text(action.cwd))
    request.resources = [...new Set(resources)].sort()
  }
  if (raw.method === 'bind_execution' && normalized.cwd !== raw.cwd) throw new Error('绑定目录不是原请求中的规范路径。请核对实际目录后重新提交。')
  if (!same(normalized, expected)) throw new Error('节点准备的请求与当前操作不同。请核对最终路径、目标和权限后重新提交。')
}
