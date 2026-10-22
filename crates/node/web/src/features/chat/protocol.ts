import { inputText } from '../workspace/model.ts'
import { newId } from '../workspace/storage.ts'
import type { MessageRequest, PendingMessage, TranscriptMessage } from './types.ts'

export type SendGate = {
  hasConversation: boolean
  loaded: boolean
  busy: boolean
}
export function roleLabel(role: string): string {
  if (role === 'user') return '你'
  if (role === 'assistant') return 'Bot'
  return role
}
export function canSend(input: SendGate): boolean {
  return Boolean(input.hasConversation && input.loaded && !input.busy)
}
export function composerHint(input: Omit<SendGate, 'busy'>): string {
  if (!input.hasConversation) return ''
  if (!input.loaded) return '稍等'
  return ''
}
export function prepareSend(input: {
  conversationId: string
  text: string
  pending: PendingMessage | null
  reportResults?: boolean
}): { ok: true; pending: PendingMessage; body: MessageRequest } | { ok: false; error: string } {
  let text: string
  try { text = inputText(input.text, '消息内容', 65536) }
  catch (caught) { return { ok: false, error: caught instanceof Error ? caught.message : '消息内容无效。' } }
  if (input.pending && (input.pending.text !== text || input.pending.conversation_id !== input.conversationId)) {
    return { ok: false, error: '上一条消息还没确认送达，先处理它再发新的。' }
  }
  const pending = input.pending ?? {
    message_id: newId(), conversation_id: input.conversationId, text,
    report_results: input.reportResults ?? true,
  }
  return { ok: true, pending, body: {
    conversation_id: pending.conversation_id, message_id: pending.message_id, text: pending.text,
    report_results: pending.report_results ?? true,
  } }
}
/** The Node admitted this exact message; a retry of the same message_id returns the same receipt. */
export function isQueuedReceipt(value: unknown, pending: PendingMessage): boolean {
  return value !== null && typeof value === 'object' && 'accepted' in value && value.accepted === true && 'message_id' in value && value.message_id === pending.message_id
}
export function replyStatusLabel(status: string | undefined): string {
  if (status === 'stopped') return '已中断'
  if (status === 'interrupted') return '已中断'
  if (status === 'failed') return '失败'
  return ''
}
export function messageKey(message: TranscriptMessage): string { return message.event_hash }

export function isSubmitKey(event: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'isComposing' | 'keyCode'>): boolean {
  return event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229
}
