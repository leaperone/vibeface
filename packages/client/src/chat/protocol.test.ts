import assert from 'node:assert/strict'
import test from 'node:test'
import { awaitsDevice, botReach, canSend, composerHint, isQueuedReceipt, isSubmitKey, prepareSend, reachDevice, replyStatusLabel, retryText, roleLabel } from './protocol.ts'
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
  const node = (online: boolean, manager_state: 'ready' | 'starting' | 'needs_setup' | null) => ({
    device_id: 'd', name: 'Mac', local: false, online, revoked: false,
    capabilities: manager_state === null ? undefined : { manager: 'codex', manager_state, relay: false, roots: [], codex_connected: false },
  })
  assert.equal(botReach([]), 'none')
  assert.equal(botReach([node(true, null)]), 'none')
  assert.equal(botReach([node(true, 'needs_setup')]), 'needs_setup')
  assert.equal(botReach([node(true, 'needs_setup'), node(true, 'starting')]), 'starting', 'a bot coming up is not a device to set up')
  assert.equal(composerHint({ ...ready, bot: 'starting' }), 'Bot 正在启动')
  assert.equal(botReach([node(false, 'needs_setup')]), 'offline')
  assert.equal(botReach([node(false, 'needs_setup'), node(true, 'needs_setup'), node(true, 'ready')]), 'ready')
  assert.equal(reachDevice([{ ...node(true, 'needs_setup'), device_id: 'setup' }, node(true, null)], 'needs_setup'), 'setup')
  assert.equal(reachDevice([node(true, null)], 'none'), undefined)
  assert.equal(awaitsDevice('none'), false, 'with no device to wait on, setting one up rereads it')
  assert.equal(awaitsDevice('ready'), false)
  assert.equal(awaitsDevice('starting'), true)
  assert.equal(awaitsDevice('needs_setup'), true)
  assert.equal(awaitsDevice('offline'), true)
})
test('a bot computer the Node lists offline with its last capabilities reads as offline, not missing', () => {
  const nodes = parseNodes({ nodes: [
    { device_id: 'here', name: 'Server', local: true, online: true, revoked: false, capabilities: { manager: null, manager_state: null, relay: true, roots: [] } },
    { device_id: 'mac', name: 'Mac', local: false, online: false, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready', relay: false, roots: ['/work'] } },
  ] })
  const reach = botReach(nodes)
  assert.equal(reach, 'offline')
  assert.equal(awaitsDevice(reach), true, 'the chat keeps rereading until the computer is back')
  assert.equal(reachDevice(nodes, reach), 'mac', 'the chat points at the computer that is away, not the one it connects through')
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

test('重试 sends the message as it was only when the reply provably did nothing', () => {
  assert.equal(retryText('改一下 README', 'missing_key', false), '改一下 README')
  assert.match(retryText('改一下 README', 'missing_key', true), /做过的不要重做/, 'a task it started may have written')
  assert.match(retryText('改一下 README', 'transport', false), /做过的不要重做/, 'it may have broken off midway')
  assert.match(retryText('改一下 README', undefined, false), /做过的不要重做/, 'a stopped reply may have done part')
})

test('a message may be only its files', () => {
  assert.equal(prepareSend({ conversationId: 'chat', text: ' ', pending: null }).ok, false)
  const files = prepareSend({ conversationId: 'chat', text: ' ', pending: null, files: true })
  assert.equal(files.ok && files.body.text, '')
})

test("a model the bot set itself is changed in its chat, not on the device", async () => {
  const { replyFailure } = await import("../lib/errors.ts")
  assert.equal(replyFailure("no_model", "model not found（这个 bot 设定的模型是 gpt-x）"), "这个 Bot 设定的模型 gpt-x 用不了。在聊天里让它换一个。")
  assert.match(replyFailure("no_model", "model not found"), /设备/)
})
