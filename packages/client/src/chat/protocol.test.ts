import assert from 'node:assert/strict'
import test from 'node:test'
import { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, isQueuedReceipt, isSubmitKey, prepareSend, replyStatusLabel, roleLabel } from './protocol.ts'
import type { ManagerJob, TurnRuntime } from '../workspace/types.ts'
import { parseNodes, parseTurnRuntime } from '../workspace/model.ts'

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
  const node = (online: boolean, manager_state: 'ready' | 'unverified' | 'starting' | 'needs_setup' | null) => ({
    device_id: 'd', name: 'Mac', local: false, online, revoked: false,
    capabilities: manager_state === null ? undefined : { manager: 'codex', manager_state, relay: false, roots: [], codex_connected: false },
  })
  assert.equal(botReach([]), 'none')
  assert.equal(botReach([node(true, null)]), 'none')
  assert.equal(botReach([node(true, 'needs_setup')]), 'needs_setup')
  assert.equal(botReach([node(true, 'unverified')]), 'unverified')
  assert.equal(botCanSend('unverified'), true)
  assert.equal(botCanSend('needs_setup'), false)
  assert.equal(composerHint({ ...ready, bot: 'unverified' }), '')
  assert.equal(botReach([node(true, 'needs_setup'), node(true, 'starting')]), 'starting', 'a bot coming up is not a device to set up')
  assert.equal(composerHint({ ...ready, bot: 'starting' }), 'Bot 正在启动')
  assert.equal(botReach([node(false, 'needs_setup')]), 'offline')
  assert.equal(botReach([node(false, 'needs_setup'), node(true, 'needs_setup'), node(true, 'ready')]), 'ready')
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

test('retry requires explicit not-started evidence and an affirmative job receipt', () => {
  const runtime: TurnRuntime = { provider: 'claude', model: null, reasoning_effort: null, generation: 'g', session_generation: 1, delivery_id: 'd', stage: 'turn_start', execution: 'not_started', attempted_at_ms: 1, switched_from: null, switch_reason: null }
  const job: ManagerJob = { trigger: 'm', state: 'failed', retryable: true, runtime }
  assert.equal(canRetryJob(job), true)
  for (const changed of [{ ...job, runtime: null }, { ...job, pending_check: true }, { ...job, retryable: false }, { ...job, state: 'reconciling' }, { ...job, runtime: { ...runtime, execution: 'unknown' as const } }, { ...job, runtime: { ...runtime, execution: 'started' as const } }]) assert.equal(canRetryJob(changed), false)
  assert.deepEqual(parseTurnRuntime(runtime), runtime)
  assert.throws(() => parseTurnRuntime({ ...runtime, execution: 'maybe' }))
  assert.match(runtimeLabel(runtime), /默认模型，实际模型未确认/)
  assert.doesNotMatch(runtimeLabel(runtime), /medium/)
  assert.match(runtimeLabel({ ...runtime, provider: 'pi', switched_from: { provider: 'claude', model: 'opus', reasoning_effort: 'high' }, switch_reason: 'quota' }), /由 Claude Code 的 opus 切换/)
})

test('the shared readiness reason names the provider and device without blocking unverified sends', () => {
  const [node] = parseNodes({ nodes: [{ device_id: 'mac', name: '我的 Mac', online: true, local: false, revoked: false, capabilities: { manager: 'claude', manager_state: 'unverified', manager_readiness: { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } }, roots: [], relay: false } }] })
  assert.equal(botCanSend(botReach([node])), true)
  assert.match(botProviderNotice([node]), /Claude Code · 我的 Mac/)
  assert.match(botProviderNotice([node]), /登录尚未确认/)
})
