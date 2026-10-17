import assert from 'node:assert/strict'
import test from 'node:test'
import { canSend, composerHint, isQueuedReceipt, isSubmitKey, prepareSend, roleLabel } from './protocol.ts'

const ready = { hasConversation: true, loaded: true, missing: false, schedulerOnline: true, schedulerAllowed: true, schedulerNodeId: 'node-1' }
test('send requires authorized configuration and permits an offline scheduler outbox', () => {
  assert.equal(roleLabel('user'), '你')
  assert.equal(roleLabel('assistant'), '调度 Agent')
  assert.equal(canSend({ ...ready, busy: false }), true)
  assert.equal(canSend({ ...ready, missing: true, busy: false }), false)
  assert.equal(canSend({ ...ready, loaded: false, busy: false }), false)
  assert.equal(canSend({ ...ready, schedulerOnline: false, busy: false }), true)
  assert.equal(canSend({ ...ready, schedulerAllowed: false, busy: false }), false)
  assert.equal(composerHint({ ...ready, missing: true }), '等待上下文补齐后再发送。')
  assert.equal(composerHint(ready), '')
  assert.match(composerHint({ ...ready, schedulerOnline: false }), /目标确认入队前会保留待核对/)
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
