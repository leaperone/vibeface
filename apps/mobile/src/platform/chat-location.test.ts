import assert from 'node:assert/strict'
import test from 'node:test'
import { chatMessageLocation, visibleChatMessageIds } from './chat-location.ts'

test('a hidden answer search hit locates its visible question card', () => {
  assert.deepEqual(chatMessageLocation({ messageId: 'a-question', messages: [{ message_id: 'a-question', answers: 'question' }], questionIds: ['question'] }), { kind: 'question', questionId: 'question' })
  assert.deepEqual(chatMessageLocation({ messageId: 'a-question', messages: [], questionIds: ['question'] }), { kind: 'question', questionId: 'question' })
})

test('an answer whose question is not loaded retains its normal message location', () => {
  assert.deepEqual(chatMessageLocation({ messageId: 'answer', messages: [{ message_id: 'answer', answers: 'missing' }], questionIds: [] }), { kind: 'message', messageId: 'answer' })
  assert.deepEqual(chatMessageLocation({ messageId: 'plain', messages: [{ message_id: 'plain' }], questionIds: ['question'] }), { kind: 'message', messageId: 'plain' })
})

test('a replacement event with the same message identity needs its own visible sample', () => {
  const prior = [{ message_id: 'same-message', event_hash: 'old-event' }]
  const replacement = [{ message_id: 'same-message', event_hash: 'new-event' }]
  assert.deepEqual(visibleChatMessageIds(prior, replacement), [])
  assert.deepEqual(visibleChatMessageIds(replacement, replacement), ['same-message'])
})
