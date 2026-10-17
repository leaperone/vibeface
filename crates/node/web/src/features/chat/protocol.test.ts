import assert from 'node:assert/strict'
import test from 'node:test'
import { canSend, composerHint, isSubmitKey, prepareSend, roleLabel } from './protocol.ts'

const ready = { hasConversation: true, loaded: true, missing: false, schedulerOnline: true, managerReady: true, schedulerNodeId: 'node-1' }
test('send stays disabled until the conversation and scheduler are ready', () => {
  assert.equal(roleLabel('user'), '你')
  assert.equal(roleLabel('assistant'), '调度 Agent')
  assert.equal(canSend({ ...ready, busy: false }), true)
  assert.equal(canSend({ ...ready, missing: true, busy: false }), false)
  assert.equal(canSend({ ...ready, loaded: false, busy: false }), false)
  assert.equal(canSend({ ...ready, schedulerOnline: false, busy: false }), false)
  assert.equal(canSend({ ...ready, managerReady: false, busy: false }), false)
  assert.equal(composerHint({ ...ready, missing: true }), '等待上下文补齐后再发送。')
})
test('an explicit retry preserves the complete original message request', () => {
  const first = prepareSend({ conversationId: 'chat', text: ' 继续  ', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: null, heads: ['head-1'] })
  assert.equal(first.ok, true)
  if (!first.ok) return
  const retry = prepareSend({ conversationId: 'chat', text: '继续', schedulerNodeId: 'node-b', entryNode: 'entry-b', pending: first.pending, heads: ['new-head'] })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.deepEqual(retry.body, first.body)
  assert.equal(retry.pending.entry_node, 'entry-a')
  assert.equal(retry.body.text, '继续')
  assert.deepEqual(retry.body.basis_heads, ['head-1'])
  assert.equal(prepareSend({ conversationId: 'chat', text: '另一句', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: first.pending, heads: [] }).ok, false)
  assert.equal(prepareSend({ conversationId: 'other-chat', text: '继续', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: first.pending, heads: [] }).ok, false)
})

test('Enter submits only after IME composition finishes and Shift Enter remains a newline', () => {
  const enter = { key: 'Enter', shiftKey: false, isComposing: false, keyCode: 13 }
  assert.equal(isSubmitKey(enter), true)
  assert.equal(isSubmitKey({ ...enter, isComposing: true }), false)
  assert.equal(isSubmitKey({ ...enter, keyCode: 229 }), false)
  assert.equal(isSubmitKey({ ...enter, shiftKey: true }), false)
  assert.equal(isSubmitKey({ ...enter, key: 'a' }), false)
})
test('invalid message content is rejected before a durable request is created', () => {
  const input = { conversationId: 'chat', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: null, heads: [] }
  assert.equal(prepareSend({ ...input, text: '  ' }).ok, false)
  assert.equal(prepareSend({ ...input, text: '中'.repeat(21846) }).ok, false)
  assert.equal(prepareSend({ ...input, text: 'a'.repeat(65536) }).ok, true)
})
