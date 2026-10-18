import { gatewayRouteApprovalHash, nodeFingerprint } from '../../generated/vibeface_secure.js'
import { parseAuthorizationTrust, typedId } from './authorization-trust'
import { integer, record, strings, text } from './node'
import { parseGatewayConnectionCard } from './connection-file'
import type { AuthorizationTrust } from './authorization-trust'
import type { ConnectionEntry, GatewayConnectionCard } from './node'

export const gatewayActions = {
  conversation_read: '读取会话', conversation_create: '创建会话', message_append: '添加消息',
  task_create: '创建工单', task_progress: '更新工单', task_accept: '接受工单', task_cancel: '取消工单',
  execution_open: '打开执行会话', execution_send: '发送执行指令', execution_steer: '补充执行指令',
  execution_interrupt: '中断执行', execution_respond_approval: '回应执行审批', execution_rebind: '重新绑定执行',
  execution_submit: '提交执行', execution_resolve: '裁定执行结果', operation_status_read: '读取操作状态',
  operation_reconcile: '核对操作结果', operation_resolve: '裁定未知结果', attachment_read: '读取附件',
  attachment_write: '写入附件', export: '导出项目', notification_read: '读取通知', notification_ack: '确认通知',
  member_manage: '管理成员', resource_manage: '管理资源', grant_issue: '签发授权', grant_revoke: '撤销授权',
  service_enroll: '登记服务', replica_read: '读取副本', agent_start: '启动 Agent',
}
export type GatewayAction = keyof typeof gatewayActions
export type GatewayRouteRequest = {
  version: 2; kind: 'vibeface-gateway-request'; client_account: string; client_key: string; target_key: string;
  resource: { space: string; resource: string }; allowed_actions: GatewayAction[]; source_pins: AuthorizationTrust['pins']
}
export type GatewayOwnerRoute = { card: string; status: 'pending' | 'installed' | 'expired' | 'superseded' }
export type GatewayOwnerInput = { file: File; password: string } & (
  | { kind: 'approve'; request: string; expiresAt: number }
  | { kind: 'revoke'; card: string; expiresAt: number }
)

export function parseGatewayRouteRequest(value: unknown): GatewayRouteRequest {
  const input = record(value, '网关申请')
  if (input.version !== 2 || input.kind !== 'vibeface-gateway-request' || Object.keys(input).some(key =>
    !['version', 'kind', 'client_account', 'client_key', 'target_key', 'resource', 'allowed_actions', 'source_pins'].includes(key))) throw new Error('网关申请格式无效，请由项目当前连接重新导出。')
  const client_account = typedId(input.client_account, 'acc2_'), client_key = typedId(input.client_key, ''), target_key = typedId(input.target_key, '')
  const resourceInput = record(input.resource), resource = { space: typedId(resourceInput.space, 'spc2_'), resource: typedId(resourceInput.resource, 'res2_') }
  if (Object.keys(resourceInput).some(key => !['space', 'resource'].includes(key)) || client_key === target_key) throw new Error('网关申请的客户端或项目范围无效。')
  const names = strings(input.allowed_actions), allowed_actions = Object.keys(gatewayActions).filter((name): name is GatewayAction => Object.hasOwn(gatewayActions, name) && names.includes(name))
  if (!names.length || names.length !== allowed_actions.length || names.some((name, index) => name !== allowed_actions[index])) throw new Error('网关申请的用途顺序无效、重复或不受支持。')
  const source_pins = parseAuthorizationTrust({ kind: 'policy', accountId: client_account, spaceId: resource.space, pins: input.source_pins }).pins
  if (!source_pins.some(pin => pin.domain.kind === 'account' && pin.domain.id === client_account)
    || !source_pins.some(pin => pin.domain.kind === 'space' && pin.domain.id === resource.space)) throw new Error('网关申请缺少申请人账户或项目空间的独立根。')
  if (source_pins.some(pin => pin.minimum_head && (pin.minimum_head.domain.kind !== pin.domain.kind || pin.minimum_head.domain.id !== pin.domain.id))) throw new Error('网关申请的政策水位属于其他独立根。')
  const domains = source_pins.map(pin => `${pin.domain.kind}:${pin.domain.id}`)
  if (domains.some((domain, index) => index > 0 && domain <= domains[index - 1])) throw new Error('网关申请的独立根顺序无效或重复。')
  return { version: 2, kind: 'vibeface-gateway-request', client_account, client_key, target_key, resource, allowed_actions, source_pins }
}

export function gatewayRequestFromCard(card: GatewayConnectionCard): GatewayRouteRequest {
  const payload = record(card.approval.payload)
  return parseGatewayRouteRequest({ version: 2, kind: 'vibeface-gateway-request', client_account: payload.client_account,
    client_key: payload.client_key, target_key: payload.target_key, resource: payload.resource,
    allowed_actions: payload.allowed_actions, source_pins: payload.source_pins })
}

export function gatewayCardJson(entry: ConnectionEntry, publicKey: string, approval: Record<string, unknown>) {
  return JSON.stringify({ version: 2, kind: 'vibeface-gateway', gateway: { url: entry.address,
    node_id: nodeFingerprint(publicKey), node_public_key: publicKey, relay: entry.relay }, approval })
}

export function parseGatewayOwnerRoute(value: unknown, publicKey: string): GatewayOwnerRoute {
  const input = record(value), card = parseGatewayConnectionCard(JSON.parse(text(input.card)))
  gatewayRouteApprovalHash(JSON.stringify(card.approval))
  if (card.gateway.nodePublicKey !== publicKey || record(card.approval.payload).gateway !== publicKey || input.status !== 'pending' && input.status !== 'installed' && input.status !== 'expired' && input.status !== 'superseded') throw new Error('已保存的网关路由不属于当前 Node。')
  return { card: text(input.card), status: input.status }
}

export type GatewayRouteStatus = { checkedAt: number } & (
  | { status: 'installed' | 'not_installed' }
  | { status: 'superseded'; currentApproval: string; currentGeneration: number }
)

export function parseGatewayRouteStatus(value: unknown, approval: Record<string, unknown>): GatewayRouteStatus {
  const response = record(value, '网关路由状态'), payload = record(approval.payload)
  const hash = gatewayRouteApprovalHash(JSON.stringify(approval)), generation = integer(payload.generation)
  if (response.route_id !== payload.route_id || response.approval !== hash || response.generation !== generation) throw new Error('网关状态属于另一份路由，请保留原待核对记录。')
  const checkedAt = integer(response.checked_at_ms)
  const allowed = ['route_id', 'approval', 'generation', 'checked_at_ms', 'status']
  if (response.status === 'superseded') {
    const currentApproval = typedId(response.current_approval, 'h2_'), currentGeneration = integer(response.current_generation)
    if (currentApproval === hash || currentGeneration < generation) throw new Error('网关返回的替代路由版本无效。')
    allowed.push('current_approval', 'current_generation')
    if (Object.keys(response).some(key => !allowed.includes(key))) throw new Error('网关路由状态包含未识别字段。')
    return { status: 'superseded', currentApproval, currentGeneration, checkedAt }
  }
  if (response.status !== 'installed' && response.status !== 'not_installed' || Object.keys(response).some(key => !allowed.includes(key))) throw new Error('网关路由状态无效。')
  return { status: response.status, checkedAt }
}

export function downloadGatewayDocument(content: string, name: string) {
  const url = URL.createObjectURL(new Blob([content], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url; link.download = `${name}.json`; link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
