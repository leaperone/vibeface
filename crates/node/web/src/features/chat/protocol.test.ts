import assert from 'node:assert/strict'
import test from 'node:test'
import { canSend, composerHint, isQueuedReceipt, isSubmitKey, prepareSend, roleLabel } from './protocol.ts'

const ready = { hasConversation: true, loaded: true, missing: false }
test('send waits for complete conversation context while the server selects the assistant', () => {
  assert.equal(roleLabel('user'), '你')
  assert.equal(roleLabel('assistant'), '助手')
  assert.equal(canSend({ ...ready, busy: false }), true)
  assert.equal(canSend({ ...ready, missing: true, busy: false }), false)
  assert.equal(canSend({ ...ready, loaded: false, busy: false }), false)
  assert.equal(composerHint({ ...ready, missing: true }), '对话还在同步。')
  assert.equal(composerHint(ready), '')
})
test('an explicit retry preserves the complete original message request', () => {
  const first = prepareSend({ conversationId: 'chat', text: ' 继续  ', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: null, heads: ['head-1'] })
  assert.equal(first.ok, true)
  if (!first.ok) return
  const retry = prepareSend({ conversationId: 'chat', text: '继续', schedulerNodeId: 'node-b', entryNode: 'entry-a', pending: first.pending, heads: ['new-head'] })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.deepEqual(retry.body, first.body)
  assert.equal(retry.pending.entry_node, 'entry-a')
  assert.equal(retry.body.entry_node, 'entry-a')
  assert.equal(retry.body.text, '继续')
  assert.deepEqual(retry.body.basis_heads, ['head-1'])
  assert.equal(prepareSend({ conversationId: 'chat', text: '另一句', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: first.pending, heads: [] }).ok, false)
  assert.equal(prepareSend({ conversationId: 'other-chat', text: '继续', schedulerNodeId: 'node-a', entryNode: 'entry-a', pending: first.pending, heads: [] }).ok, false)
  assert.equal(prepareSend({ conversationId: 'chat', text: '继续', schedulerNodeId: 'node-a', entryNode: 'entry-b', pending: first.pending, heads: [] }).ok, false)
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
test('report authorization survives retries and only the exact durable queue receipt settles a message', () => {
  const prepared = prepareSend({ conversationId: 'chat', text: '后台处理', schedulerNodeId: 'scheduler', entryNode: 'entry', heads: [], pending: null, reportResults: true })
  assert.equal(prepared.ok, true)
  if (!prepared.ok) return
  const retry = prepareSend({ conversationId: 'chat', text: '后台处理', schedulerNodeId: 'other', entryNode: 'entry', heads: ['new'], pending: prepared.pending, reportResults: false })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.deepEqual(retry.body, prepared.body)
  assert.equal(retry.body.report_results, true)
  const receipt = { accepted: true, queued: true, message_id: prepared.pending.message_id, scheduler_node: 'scheduler' }
  assert.equal(isQueuedReceipt(receipt, prepared.pending), true)
  for (const value of [{ accepted: true }, { ...receipt, queued: false }, { ...receipt, message_id: 'other' }, { ...receipt, scheduler_node: 'other' }]) assert.equal(isQueuedReceipt(value, prepared.pending), false)
})

test('an automatically routed message settles only after the server confirms its message identity and route', () => {
  const prepared = prepareSend({ conversationId: 'chat', text: '查询进展', schedulerNodeId: null, entryNode: 'entry', pending: null, heads: [] })
  assert.equal(prepared.ok, true)
  if (!prepared.ok) return
  assert.equal(prepared.body.scheduler_node, null)
  const receipt = { accepted: true, queued: true, message_id: prepared.pending.message_id, scheduler_node: 'chosen' }
  assert.equal(isQueuedReceipt(receipt, prepared.pending), true)
  assert.equal(isQueuedReceipt({ ...receipt, message_id: 'other' }, prepared.pending), false)
  assert.equal(isQueuedReceipt({ ...receipt, scheduler_node: '' }, prepared.pending), false)
  assert.equal(isQueuedReceipt(receipt, { ...prepared.pending, scheduler_node: 'original' }), false)
})
