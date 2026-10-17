import { array, record, same, text } from './node'
import type { RpcFacts } from './node'

export function prepareContext(body: Record<string, unknown>, audienceDeviceId: string, resourceId: string, facts?: RpcFacts) {
  const request = body.request === undefined ? null : record(body.request)
  const conversation = body.conversation_id ?? request?.conversation_id ?? null
  const taskId = body.task_id ?? request?.task_id
  if (taskId !== undefined && body.method !== 'create_task' && !facts?.taskRef) throw new Error('此请求缺少本地已验证的工单创建证明。')
  if (facts?.taskRef !== undefined && !/^[a-f0-9]{64}$/.test(facts.taskRef)) throw new Error('工单事件引用无效。')
  const execution = facts?.execution
  if ((body.session_id !== undefined || request?.session_id !== undefined) && !execution) throw new Error('此请求缺少本地已验证的执行会话。')
  return {
    context: {
      conversation: conversation === null ? null : text(conversation), task: facts?.taskRef ?? null, sdk_session: null,
      execution_session: execution ? { target: audienceDeviceId, session_id: execution.sessionId } : null,
      executor_kind: execution?.executorKind ?? null, workspace_resource: resourceId,
    },
    binding_generation: execution?.bindingGeneration ?? null, canonical_cwd: execution?.canonicalCwd ?? null,
  }
}

export function verifyPreparedBody(raw: Record<string, unknown>, normalized: Record<string, unknown>, facts?: RpcFacts) {
  const expected = record(JSON.parse(JSON.stringify(raw)))
  if (raw.method === 'update_task' && raw.detail === undefined) expected.detail = ''
  if (raw.method === 'operate') {
    const request = record(expected.request), finalRequest = record(normalized.request), action = record(request.action)
    if (action.type === 'open' && record(finalRequest.action).cwd !== action.cwd) throw new Error('节点报告了不同的实际工作目录。请核对规范路径后重新提交。')
    const resources = array(request.resources).map(value => text(value))
    if (action.type === 'open' || action.type === 'send') {
      const cwd = facts?.execution?.canonicalCwd
      if (!cwd) throw new Error('无法独立确认执行资源的工作目录。')
      resources.push(cwd)
    }
    request.resources = [...new Set(resources)].sort()
  }
  if (raw.method === 'bind_execution' && normalized.cwd !== raw.cwd) throw new Error('绑定目录不是原请求中的规范路径。请核对实际目录后重新提交。')
  if (!same(normalized, expected)) throw new Error('节点准备的请求与当前操作不同。请核对最终路径、目标和权限后重新提交。')
}
