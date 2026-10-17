import { newId } from '../workspace/storage.ts'
import type { MessageRequest, PendingMessage, TranscriptMessage } from './types.ts'

export type SendGate = {
  hasConversation: boolean
  loaded: boolean
  missing: boolean
  schedulerOnline: boolean
  managerReady: boolean
  schedulerNodeId: string | null
  busy: boolean
}
export function roleLabel(role: string): string {
  if (role === 'user') return '你'
  if (role === 'assistant') return '调度 Agent'
  return role
}
export function canSend(input: SendGate): boolean {
  return Boolean(input.hasConversation && input.loaded && !input.missing && input.schedulerOnline && input.managerReady && input.schedulerNodeId && !input.busy)
}
export function composerHint(input: Omit<SendGate, 'busy'>): string {
  if (!input.hasConversation || !input.loaded) return '先创建或选择一个会话。'
  if (input.missing) return '等待上下文补齐后再发送。'
  if (!input.managerReady || !input.schedulerNodeId) return '设备尚未配置调度 Agent。执行会话仍可直接操作。'
  if (!input.schedulerOnline) return '所选调度设备离线，请切换其他入口或设备。'
  return '消息将交给所选设备上的调度 Agent。'
}
export function prepareSend(input: {
  conversationId: string
  text: string
  schedulerNodeId: string
  entryNode: string
  pending: PendingMessage | null
  heads: string[]
}): { ok: true; pending: PendingMessage; body: MessageRequest } | { ok: false; error: string } {
  const text = input.text.trim()
  if (!text) return { ok: false, error: '先填写消息内容。' }
  if (input.pending && (input.pending.text !== text || input.pending.conversation_id !== input.conversationId)) {
    return { ok: false, error: '此会话有一条消息尚未确认，请先核对原消息，再发送新内容。' }
  }
  const pending = input.pending ?? {
    message_id: newId(), conversation_id: input.conversationId, text,
    scheduler_node: input.schedulerNodeId, entry_node: input.entryNode, basis_heads: [...input.heads],
  }
  return { ok: true, pending, body: {
    conversation_id: pending.conversation_id, message_id: pending.message_id, text: pending.text,
    basis_heads: pending.basis_heads, scheduler_node: pending.scheduler_node,
  } }
}
export function messageKey(message: TranscriptMessage): string { return message.event_hash }
