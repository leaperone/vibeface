import { createConversationCache, type CachedConversation } from '@vibeface/client/workspace/conversation-cache.ts'
import { chatTimeline } from '@vibeface/client/chat/timeline.ts'
import { foldAnswers } from '@vibeface/client/workspace/questions.ts'
import type { PendingMessage } from '@vibeface/client/chat/types.ts'
import type { Conversation } from './types'
import { mobilePlatform } from '@/platform'

export const localConversations = createConversationCache({ storage: mobilePlatform.storage })

export function cachedConversation(record: CachedConversation, pending: readonly PendingMessage[] = []): Conversation {
  const { payload } = record
  const timeline = chatTimeline(payload.messages, payload.questions, new Set())
  const folded = foldAnswers(payload.messages, payload.questions)
  const messages: Conversation['messages'] = [...timeline.messages]
  for (const item of pending) if (item.conversation_id === record.conversationId && item.target.kind === 'bot_main' && !messages.some((message) => message.message_id === item.message_id)) {
    messages.push({ message_id: item.message_id, event_hash: item.message_id, role: 'user', text: item.text, attachments: item.attachments, created_at_ms: item.created_at_ms, state: 'sending' })
  }
  return {
    messages, before: timeline.before, rest: timeline.rest, answers: folded.answers, hidden: folded.hidden,
    unread: null, works: [], replying: false, status: null, stoppable: false,
    blocked: '本地记录可查看，连上并同步后继续发送。', origin: { kind: 'cache', savedAt: record.savedAt },
  }
}
