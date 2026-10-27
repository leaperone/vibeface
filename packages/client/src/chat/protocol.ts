import { inputText } from '../workspace/model.ts'
import { newId } from '../workspace/storage.ts'
import type { MessageRequest, PendingMessage, TranscriptMessage } from './types.ts'

/** An empty chat's first messages: project questions for a bot with a folder, otherwise what any bot can do from chat. */
export const starters = { project: ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？'], general: ['你能帮我做什么？', '每天早上 9 点提醒我', '记住：回复用中文'] }

type SendGate = {
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

export function isSubmitKey(event: { key: string; shiftKey: boolean; isComposing: boolean; keyCode: number }): boolean {
  return event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229
}

/**
 * Each reply follows the message it answers, even when the person wrote again before that reply finished.
 * A `joined` message went into the reply then running, so it sits before that reply, after the message it answers.
 */
export function threadReplies<T extends Pick<TranscriptMessage, 'message_id' | 'role' | 'in_reply_to'>>(messages: T[], joined: { has(id: string): boolean } = new Set()): T[] {
  const ids = new Set(messages.map((message) => message.message_id))
  const answers = (message: T) => message.role === 'assistant' && message.in_reply_to !== undefined && ids.has(message.in_reply_to) ? message.in_reply_to : null
  const replies = new Map<string, T[]>(), merged = new Map<string, T[]>(), moved = new Set<string>()
  let head: string | null = null
  for (const message of messages) {
    const to = answers(message)
    if (to) replies.set(to, [...replies.get(to) ?? [], message])
    else if (head && joined.has(message.message_id)) { merged.set(head, [...merged.get(head) ?? [], message]); moved.add(message.message_id) }
    else head = message.message_id
  }
  const thread = (message: T): T[] => [message, ...replies.get(message.message_id) ?? []]
  return messages.flatMap((message) => answers(message) || moved.has(message.message_id) ? []
    : [message, ...(merged.get(message.message_id) ?? []).flatMap(thread), ...replies.get(message.message_id) ?? []])
}
