import { inputText } from '../workspace/model.ts'
import { newId } from '../workspace/storage.ts'
import type { MessageRequest, PendingMessage, TranscriptMessage } from './types.ts'

export type SendGate = {
  hasConversation: boolean
  loaded: boolean
  missing: boolean
  busy: boolean
}
export function roleLabel(role: string): string {
  if (role === 'user') return '你'
  if (role === 'assistant') return '助手'
  return role
}
export function canSend(input: SendGate): boolean {
  return Boolean(input.hasConversation && input.loaded && !input.missing && !input.busy)
}
export function composerHint(input: Omit<SendGate, 'busy'>): string {
  if (!input.hasConversation) return ''
  if (!input.loaded) return '稍等'
  if (input.missing) return '对话还在同步。'
  return ''
}
export function prepareSend(input: {
  conversationId: string
  text: string
  schedulerPeerId: string | null
  entryNode: string
  pending: PendingMessage | null
  heads: string[]
  reportResults?: boolean
}): { ok: true; pending: PendingMessage; body: MessageRequest } | { ok: false; error: string } {
  let text: string
  try { text = inputText(input.text, '消息内容', 65536) }
  catch (caught) { return { ok: false, error: caught instanceof Error ? caught.message : '消息内容无效。' } }
  if (input.pending && (input.pending.text !== text || input.pending.conversation_id !== input.conversationId)) {
    return { ok: false, error: '上一条消息还没确认送达，先处理它再发新的。' }
  }
  if (input.pending && input.pending.entry_node !== input.entryNode) {
    return { ok: false, error: '上一条消息还没确认，要回到原来的设备上处理。' }
  }
  const pending = input.pending ?? {
    message_id: newId(), conversation_id: input.conversationId, text,
    scheduler_node: input.schedulerPeerId, entry_node: input.entryNode, basis_heads: [...input.heads],
    report_results: input.reportResults ?? true,
  }
  return { ok: true, pending, body: {
    conversation_id: pending.conversation_id, message_id: pending.message_id, text: pending.text,
    basis_heads: pending.basis_heads, scheduler_node: pending.scheduler_node,
    entry_node: pending.entry_node,
    report_results: pending.report_results ?? true,
  } }
}
export function isQueuedReceipt(value: unknown, pending: PendingMessage): boolean {
  return value !== null && typeof value === 'object' && 'accepted' in value && value.accepted === true && 'queued' in value && value.queued === true && 'message_id' in value && value.message_id === pending.message_id && 'scheduler_node' in value && typeof value.scheduler_node === 'string' && value.scheduler_node.length > 0 && (pending.scheduler_node === null || value.scheduler_node === pending.scheduler_node)
}
export function messageKey(message: TranscriptMessage): string { return message.event_hash }

export function isSubmitKey(event: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'isComposing' | 'keyCode'>): boolean {
  return event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229
}
