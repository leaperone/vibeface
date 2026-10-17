export function rpcAction(method: string, body: Record<string, unknown>) {
  switch (method) {
    case 'conversations':
    case 'conversation': return 'conversation_read'
    case 'notifications': return 'notification_read'
    case 'create_conversation': return 'conversation_create'
    case 'message': return 'message_append'
    case 'create_task': return 'task_create'
    case 'update_task': return body.status === 'accepted' ? 'task_accept' : body.status === 'cancelled' ? 'task_cancel' : 'task_progress'
    case 'nodes':
    case 'operation':
    case 'session': return 'operation_status_read'
    case 'reconcile': return 'operation_reconcile'
    case 'resolve_unknown': return 'operation_resolve'
    case 'ack': return 'notification_ack'
    case 'rebind_execution': return 'execution_rebind'
    case 'export': return 'export'
    case 'operate': {
      const request = body.request
      if (!request || typeof request !== 'object' || !('action' in request)) throw new Error('执行请求缺少操作内容。')
      const action = request.action
      if (!action || typeof action !== 'object' || !('type' in action)) throw new Error('执行请求缺少操作类型。')
      switch (action.type) {
        case 'open': return 'execution_open'
        case 'read': return 'operation_status_read'
        case 'send': return 'execution_send'
        case 'steer': return 'execution_steer'
        case 'interrupt': return 'execution_interrupt'
        case 'respond_request': return 'execution_respond_approval'
        default: throw new Error('执行请求包含未支持的操作。')
      }
    }
    default: throw new Error('此客户端不支持为该请求签名。')
  }
}

function stableAuthorizationId(prefix: string, value: unknown) {
  if (typeof value !== 'string' || !value) throw new Error('请求缺少原始业务编号。')
  return `${prefix}:${value}`
}

export function prepareRpc(input: { method: string; params: Record<string, unknown>; spaceId: string; authorizationId: string }) {
  const { method, params, spaceId } = input
  if (params.space_id !== undefined && params.space_id !== spaceId || params.method !== undefined && params.method !== method || params.authorization_id !== undefined) throw new Error('请求方法、空间或授权编号与当前操作不一致。')
  let authorizationId = input.authorizationId
  switch (method) {
    case 'operate': {
      const request = params.request
      if (!request || typeof request !== 'object' || !('operation_id' in request) || typeof request.operation_id !== 'string') throw new Error('执行请求缺少原操作编号。')
      authorizationId = request.operation_id
      break
    }
    case 'message': authorizationId = stableAuthorizationId('message', params.message_id); break
    case 'create_conversation': authorizationId = stableAuthorizationId('conversation', params.conversation_id); break
    case 'create_task': authorizationId = stableAuthorizationId('task', params.task_id); break
    case 'ack': authorizationId = stableAuthorizationId('ack', params.notification_id); break
  }
  if (!/^[A-Za-z0-9_:.\-]{1,128}$/.test(authorizationId)) throw new Error('授权调用编号无效。请保留原请求内容。')
  const body = { ...params, method, space_id: spaceId, authorization_id: authorizationId }
  return { body, action: rpcAction(method, body), authorizationId }
}
