import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import { answerId } from '@vibeface/client/workspace/questions.ts'

export function chatMessageLocation(input: { messageId: string; messages: readonly Pick<TranscriptMessage, 'message_id' | 'answers'>[]; questionIds: readonly string[] }): { kind: 'message'; messageId: string } | { kind: 'question'; questionId: string } {
  const answered = input.messages.find((message) => message.message_id === input.messageId)?.answers
  const questionId = input.questionIds.find((id) => id === answered || answerId(id) === input.messageId)
  return questionId ? { kind: 'question', questionId } : { kind: 'message', messageId: input.messageId }
}
