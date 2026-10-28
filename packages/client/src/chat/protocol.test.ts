import assert from 'node:assert/strict'
import test from 'node:test'
import { awaitsDevice, botReach, canSend, composerHint, isQueuedReceipt, isSubmitKey, prepareSend, replyStatusLabel, roleLabel, threadReplies } from './protocol.ts'
import { parseNodes } from '../workspace/model.ts'

const ready = { hasConversation: true, loaded: true, bot: 'ready' as const }
test('send waits for a loaded conversation', () => {
  assert.equal(roleLabel('user'), '你')
  assert.equal(roleLabel('assistant'), 'Bot')
  assert.equal(canSend({ ...ready, busy: false }), true)
  assert.equal(canSend({ ...ready, loaded: false, busy: false }), false)
  assert.equal(composerHint(ready), '')
  assert.equal(composerHint({ ...ready, bot: 'needs_setup' }), '运行 Bot 的设备需设置')
})
test('chat rereads readiness only while a device that could reply is not ready yet', () => {
  const node = (online: boolean, manager_ready: boolean | null) => ({
    device_id: 'd', name: 'Mac', local: false, online, revoked: false,
    capabilities: manager_ready === null ? undefined : { manager: 'codex', manager_ready, relay: false, roots: [], codex_connected: false },
  })
  assert.equal(botReach([]), 'none')
  assert.equal(botReach([node(true, null)]), 'none')
  assert.equal(botReach([node(true, false)]), 'needs_setup')
  assert.equal(botReach([node(false, false)]), 'offline')
  assert.equal(botReach([node(false, false), node(true, false), node(true, true)]), 'ready')
  assert.equal(awaitsDevice('none'), false, 'with no device to wait on, setting one up rereads it')
  assert.equal(awaitsDevice('ready'), false)
  assert.equal(awaitsDevice('needs_setup'), true)
  assert.equal(awaitsDevice('offline'), true)
})
test('a bot computer the Node lists offline with its last capabilities reads as offline, not missing', () => {
  const nodes = parseNodes({ nodes: [
    { device_id: 'here', name: 'Server', local: true, online: true, revoked: false, capabilities: { manager: null, manager_ready: false, relay: true, roots: [] } },
    { device_id: 'mac', name: 'Mac', local: false, online: false, revoked: false, capabilities: { manager: 'codex', manager_ready: true, relay: false, roots: ['/work'] } },
  ] })
  const reach = botReach(nodes)
  assert.equal(reach, 'offline')
  assert.equal(awaitsDevice(reach), true, 'the chat keeps rereading until the computer is back')
  assert.equal(composerHint({ hasConversation: true, loaded: true, bot: reach }), '能运行 Bot 的设备离线了')
})
test('an explicit retry preserves the complete original message request', () => {
  const first = prepareSend({ conversationId: 'chat', text: ' 继续  ', pending: null })
  assert.equal(first.ok, true)
  if (!first.ok) return
  const retry = prepareSend({ conversationId: 'chat', text: '继续', pending: first.pending })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.deepEqual(retry.body, first.body)
  assert.equal(retry.body.text, '继续')
  assert.equal(prepareSend({ conversationId: 'chat', text: '另一句', pending: first.pending }).ok, false)
  assert.equal(prepareSend({ conversationId: 'other-chat', text: '继续', pending: first.pending }).ok, false)
  assert.equal(prepareSend({ conversationId: 'chat', text: '继续', pending: first.pending }).ok, true, 'another entrance resends the same request')
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
  const input = { conversationId: 'chat', pending: null }
  assert.equal(prepareSend({ ...input, text: '  ' }).ok, false)
  assert.equal(prepareSend({ ...input, text: '中'.repeat(21846) }).ok, false)
  assert.equal(prepareSend({ ...input, text: 'a'.repeat(65536) }).ok, true)
})
test('report results default on and survive retries; only the exact admission receipt settles a message', () => {
  const prepared = prepareSend({ conversationId: 'chat', text: '后台处理', pending: null, reportResults: true })
  assert.equal(prepared.ok, true)
  if (!prepared.ok) return
  const retry = prepareSend({ conversationId: 'chat', text: '后台处理', pending: prepared.pending, reportResults: false })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.deepEqual(retry.body, prepared.body)
  assert.equal(retry.body.report_results, true)
  const receipt = { accepted: true, message_id: prepared.pending.message_id, state: 'queued' }
  assert.equal(isQueuedReceipt(receipt, prepared.pending), true)
  for (const value of [{ accepted: true }, { ...receipt, accepted: false }, { ...receipt, message_id: 'other' }]) assert.equal(isQueuedReceipt(value, prepared.pending), false)
})
test('stopped and interrupted replies are labelled; completed replies are not', () => {
  assert.equal(replyStatusLabel('completed'), '')
  assert.equal(replyStatusLabel(undefined), '')
  assert.equal(replyStatusLabel('stopped'), '已中断')
  assert.equal(replyStatusLabel('interrupted'), '已中断')
})
test('a reply follows the message it answers when the next message was sent before it finished', () => {
  const rows = [
    { message_id: 'a', role: 'user' },
    { message_id: 'b', role: 'user' },
    { message_id: 'ra', role: 'assistant', in_reply_to: 'a' },
    { message_id: 'rb', role: 'assistant', in_reply_to: 'b' },
    { message_id: 'answer', role: 'assistant', in_reply_to: 'question-1' },
  ]
  assert.deepEqual(threadReplies(rows).map((row) => row.message_id), ['a', 'ra', 'b', 'rb', 'answer'])
})
test('a message that joined the running reply sits before that reply', () => {
  const rows = [
    { message_id: 'a', role: 'user' },
    { message_id: 'j1', role: 'user' },
    { message_id: 'j2', role: 'user' },
    { message_id: 'ra', role: 'assistant', in_reply_to: 'a' },
    { message_id: 'b', role: 'user' },
    { message_id: 'rb', role: 'assistant', in_reply_to: 'b' },
  ]
  assert.deepEqual(threadReplies(rows, new Set(['j1', 'j2'])).map((row) => row.message_id), ['a', 'j1', 'j2', 'ra', 'b', 'rb'])
})
