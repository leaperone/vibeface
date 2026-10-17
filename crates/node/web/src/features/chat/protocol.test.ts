import assert from 'node:assert/strict'
import test from 'node:test'

import { canSend, composerHint, isIncomplete, pendingAfterFailure, prepareSend, roleLabel } from './protocol.ts'

test('roles use the existing labels', () => {
  assert.equal(roleLabel('user'), '你')
  assert.equal(roleLabel('assistant'), '调度 Agent')
  assert.equal(roleLabel('system'), 'system')
})

test('send stays disabled until the conversation and scheduler are ready', () => {
  const ready = {
    hasConversation: true,
    loaded: true,
    missing: false,
    schedulerOnline: true,
    managerReady: true,
    schedulerNodeId: 'node-1',
  }
  assert.equal(canSend({ ...ready, busy: false }), true)
  assert.equal(canSend({ ...ready, missing: true, busy: false }), false)
  assert.equal(canSend({ ...ready, hasConversation: false, busy: false }), false)
  assert.equal(canSend({ ...ready, schedulerOnline: false, busy: false }), false)
  assert.equal(canSend({ ...ready, managerReady: false, busy: false }), false)
  assert.equal(canSend({ ...ready, schedulerNodeId: null, busy: false }), false)
  assert.equal(composerHint({ ...ready, missing: true }), '等待上下文补齐后再发送。')
  assert.equal(composerHint({ ...ready, hasConversation: false }), '先创建或选择一个会话。')
})

test('an unknown result keeps the message id and a rejected write drops it', () => {
  const first = prepareSend({
    conversationId: 'chat',
    text: ' 继续  ',
    schedulerNodeId: 'node-a',
    pending: null,
    heads: ['head-1'],
  })
  assert.equal(first.ok, true)
  if (!first.ok) return
  assert.equal(first.body.text, '继续')
  assert.deepEqual(first.body.basis_heads, ['head-1'])
  assert.equal(pendingAfterFailure(undefined), 'keep')
  assert.equal(pendingAfterFailure(503), 'keep')
  assert.equal(pendingAfterFailure(409), 'keep')
  assert.equal(pendingAfterFailure(400), 'drop')
  const retry = prepareSend({
    conversationId: 'chat',
    text: '继续',
    schedulerNodeId: 'node-b',
    pending: first.pending,
    heads: ['head-1'],
  })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.equal(retry.body.message_id, first.body.message_id)
  assert.equal(retry.body.scheduler_node, 'node-a')
  const changed = prepareSend({
    conversationId: 'chat',
    text: '另一句',
    schedulerNodeId: 'node-a',
    pending: first.pending,
    heads: [],
  })
  assert.equal(changed.ok, false)
})

test('missing context counts events, blobs, and conflicts', () => {
  assert.equal(isIncomplete({ events: ['a'], blobs: [], conflicts: [] }), true)
  assert.equal(isIncomplete({ events: 0, blobs: 0, conflicts: 2 }), true)
  assert.equal(isIncomplete({ events: [], blobs: [], conflicts: [] }), false)
})
