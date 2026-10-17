import { inputText } from '../workspace/model.ts'
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
  return Boolean(input.hasConversation && input.loaded && !input.missing && input.managerReady && input.schedulerNodeId && !input.busy)
}
export function composerHint(input: Omit<SendGate, 'busy'>): string {
  if (!input.hasConversation || !input.loaded) return '先创建或选择一个会话。'
  if (input.missing) return '等待上下文补齐后再发送。'
  if (!input.managerReady || !input.schedulerNodeId) return '设备尚未配置调度 Agent。执行会话仍可直接操作。'
  if (!input.schedulerOnline) return '调度设备当前离线；入口可先保存原消息，目标确认入队前会保留待核对。'
  return ''
}
export function prepareSend(input: {
  conversationId: string
  text: string
  schedulerNodeId: string
  entryNode: string
  pending: PendingMessage | null
  heads: string[]
  reportResults?: boolean
}): { ok: true; pending: PendingMessage; body: MessageRequest } | { ok: false; error: string } {
  let text: string
  try { text = inputText(input.text, '消息内容', 65536) }
  catch (caught) { return { ok: false, error: caught instanceof Error ? caught.message : '消息内容无效。' } }
  if (input.pending && (input.pending.text !== text || input.pending.conversation_id !== input.conversationId)) {
    return { ok: false, error: '此会话有一条消息尚未确认，请先核对原消息，再发送新内容。' }
  }
  const pending = input.pending ?? {
    message_id: newId(), conversation_id: input.conversationId, text,
    scheduler_node: input.schedulerNodeId, entry_node: input.entryNode, basis_heads: [...input.heads],
    report_results: input.reportResults ?? false,
  }
  return { ok: true, pending, body: {
    conversation_id: pending.conversation_id, message_id: pending.message_id, text: pending.text,
    basis_heads: pending.basis_heads, scheduler_node: pending.scheduler_node,
    report_results: pending.report_results ?? false,
  } }
}
export function isQueuedReceipt(value: unknown, pending: PendingMessage): boolean {
  return value !== null && typeof value === 'object' && 'accepted' in value && value.accepted === true && 'queued' in value && value.queued === true && 'message_id' in value && value.message_id === pending.message_id && 'scheduler_node' in value && value.scheduler_node === pending.scheduler_node
}
export function messageKey(message: TranscriptMessage): string { return message.event_hash }

export function isSubmitKey(event: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'isComposing' | 'keyCode'>): boolean {
  return event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229
}
