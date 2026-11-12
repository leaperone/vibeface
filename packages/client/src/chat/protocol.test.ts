import assert from 'node:assert/strict'
import test from 'node:test'
import { awaitsDevice, botReach, botCanSend, canRetryJob, inputEventBody, inputTargetKey, inputTaskCommand, runtimeLabel, canSend, composerHint, isQueuedReceipt, isSubmitKey, prepareSend, promptIntent, replyStatusLabel, retryText, reachDevice } from './protocol.ts'
import type { ManagerJob, TurnRuntime } from '../workspace/types.ts'
import { parseInputReceipt, parseMessageRequest, parseNodes, parsePendingMessage, parseTurnRuntime, parseWorkspaceCommand, parseWorkspaceCommandNotSaved, parseWorkspaceHistory, parseWorkspaceState, record } from '../workspace/model.ts'
import { enqueue, type OutboxItem } from '../lib/outbox.ts'

const prepareMainSend = (input: Omit<Parameters<typeof prepareSend>[0], 'target' | 'sequence'>) => prepareSend({ ...input, target: { kind: 'bot_main', bot_id: 'bot' }, sequence: 1 })

const ready = { hasConversation: true, loaded: true, bot: 'ready' as const }
test('a command refusal settles only the exact explicitly unsaved write', () => {
  const commandId = 'c'.repeat(64)
  const refusal = { accepted: false, code: 'workspace_command_not_saved', stored: false, command_id: commandId, reason: '任务已更新，本次操作未保存。' }
  assert.deepEqual(parseWorkspaceCommandNotSaved(refusal, commandId), refusal)
  assert.equal(parseWorkspaceCommandNotSaved({ ...refusal, reason: '' }, commandId)?.reason, '')
  for (const changed of [{ command_id: 'd'.repeat(64) }, { command_id: undefined }, { accepted: true }, { stored: true }, { stored: undefined }, { reason: undefined }]) assert.throws(() => parseWorkspaceCommandNotSaved({ ...refusal, ...changed }, commandId))
  assert.equal(parseWorkspaceCommandNotSaved({ ...refusal, code: 'forward_error' }, commandId), null)
  assert.equal(parseWorkspaceCommandNotSaved({ responded: true, message: 'ACK lost' }, commandId), null)
})
test('automatic Task commands meet the Node expected-result contract and retain their source', () => {
  const sources = ['a'.repeat(64), 'b'.repeat(64)]
  for (const input of [
    { task_id: 'main-task', owner_bot: 'bot', text: '  检查项目  ' },
    { task_id: 'channel-task', owner_bot: 'bot', channel_id: 'channel', text: '给出频道调查结论' },
    { task_id: 'work-task', owner_bot: 'bot', text: '先看原工作结果，再继续完成', source_events: sources },
    { task_id: 'attachment-task', owner_bot: 'bot', text: '' },
  ]) {
    const command = inputTaskCommand(input)
    assert.equal(command.task_id, input.task_id)
    assert.equal(command.request, input.text.trim() || '附件输入')
    assert.ok(command.expected_result.trim().length > 0)
    assert.ok(Buffer.byteLength(command.expected_result, 'utf8') <= 16384)
    assert.deepEqual(command.source_events, 'source_events' in input ? sources : [])
  }
  assert.deepEqual(sources, ['a'.repeat(64), 'b'.repeat(64)])
})
test('send waits for a loaded conversation', () => {
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
  assert.equal(botReach([node(true, 'needs_setup'), node(true, 'unverified')]), 'unverified')
  assert.equal(composerHint({ ...ready, bot: 'unverified' }), '', 'an unverified provider can be tried')
  assert.equal(awaitsDevice('unverified'), false, 'unknown authentication is not a pending launch')
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
  const first = prepareMainSend({ conversationId: 'chat', text: ' 继续  ', pending: null })
  assert.equal(first.ok, true)
  if (!first.ok) return
  const retry = prepareMainSend({ conversationId: 'chat', text: '继续', pending: first.pending })
  assert.equal(retry.ok, true)
  if (!retry.ok) return
  assert.deepEqual(retry.body, first.body)
  assert.equal(retry.body.text, '继续')
  assert.equal(prepareMainSend({ conversationId: 'chat', text: '另一句', pending: first.pending }).ok, false)
  assert.equal(prepareMainSend({ conversationId: 'other-chat', text: '继续', pending: first.pending }).ok, false)
  assert.equal(prepareMainSend({ conversationId: 'chat', text: '继续', pending: first.pending }).ok, true, 'another entrance resends the same request')
})
test('three inputs for one target keep independent IDs and signed target order', () => {
  const target = { kind: 'channel_task', channel_id: 'channel', task_id: 'task', bot_id: 'bot' } satisfies Parameters<typeof prepareSend>[0]['target']
  const prepared = [1, 2, 3].map((sequence) => prepareSend({ conversationId: 'chat', target, sequence, expected_revision: 4, text: `补充${sequence}` }))
  const accepted = prepared.flatMap((item) => item.ok ? [item] : [])
  assert.equal(accepted.length, 3)
  assert.equal(new Set(accepted.map((item) => item.pending.message_id)).size, 3)
  assert.deepEqual(accepted.map((item) => inputEventBody(item.body)), accepted.map((item, index) => ({ type: 'input_posted', message_id: item.pending.message_id, target, intent: 'continue', sequence: index + 1, expected_revision: 4, text: `补充${index + 1}`, report_results: true })))
  assert.notEqual(inputTargetKey(target), inputTargetKey({ ...target, task_id: 'sibling' }))
  const deferred = prepareSend({ conversationId: 'chat', target, sequence: 4, expected_revision: 4, text: '稍后处理', intent: 'defer' })
  assert.equal(deferred.ok && deferred.body.target, target)
  assert.equal(deferred.ok && deferred.body.intent, 'defer')
  assert.equal(prepareSend({ conversationId: 'chat', target, sequence: 0, text: '无效顺序' }).ok, false)
})
test('a persisted unknown input retains the original signature, attachment bytes and target', async () => {
  const prepared = prepareSend({ conversationId: 'chat', target: { kind: 'task', task_id: 'task' }, sequence: 1, expected_revision: 2, text: '', files: true })
  assert.equal(prepared.ok, true)
  if (!prepared.ok) return
  const attachment = { hash: 'a'.repeat(64), size: 3, name: 'note.txt' }
  const item: OutboxItem = { key: prepared.pending.message_id, method: 'message', params: { ...prepared.body, attachments: [attachment], attestation: { device_id: 'client', signature: 'original-signature' } }, created_at_ms: 1, delivery: 'unknown', uploads: [{ ...attachment, data: 'YWJj' }] }
  const entries = new Map<string, OutboxItem>()
  const store = { all: async () => [...entries.values()], put: async (item: OutboxItem) => { entries.set(item.key, structuredClone(item)) }, delete: async (key: string) => { entries.delete(key) } }
  await enqueue(store, item)
  assert.deepEqual(await enqueue(store, { ...item, delivery: 'saved', uploads: [] }), item)
  const restored = parsePendingMessage((await store.all())[0])
  assert.equal(restored.message_id, item.params.message_id)
  assert.equal(restored.delivery, 'unknown')
  assert.deepEqual(restored.target, { kind: 'task', task_id: 'task' })
  assert.deepEqual(restored.attachments, [attachment])
  assert.deepEqual((await store.all())[0].params, item.params)
  assert.deepEqual((await store.all())[0].uploads, item.uploads)
  const releasing = parsePendingMessage({ ...item, release_expected_event: 'a'.repeat(64), release_command_id: 'c'.repeat(64) })
  assert.equal(releasing.release_expected_event, 'a'.repeat(64))
  assert.equal(releasing.release_command_id, 'c'.repeat(64))
  const receipt = { message_id: restored.message_id, target: restored.target, stage: 'provider_accepted', actual_turn_id: null, reason: null, retry_safe: false, control_event: null }
  assert.equal(parseInputReceipt(receipt).actual_turn_id, null)
  assert.equal(parseInputReceipt(receipt).control_event, null)
  const safe = { ...receipt, stage: 'refused', retry_safe: true, control_event: 'a'.repeat(64) }
  assert.equal(parseInputReceipt(safe).control_event, safe.control_event)
  assert.equal(parseInputReceipt({ ...safe, control_event: 'A'.repeat(64) }).control_event, 'A'.repeat(64))
  for (const control_event of [undefined, null, 1, '', 'a'.repeat(63), 'g'.repeat(64)]) assert.throws(() => parseInputReceipt({ ...safe, control_event }))
  assert.throws(() => parseInputReceipt({ ...receipt, control_event: undefined }))
  for (const rejection_code of ['input_not_saved', 'input_conflict'] satisfies NonNullable<OutboxItem['rejection_code']>[]) assert.equal(parsePendingMessage({ ...item, rejection_code }).rejection_code, rejection_code)
  assert.throws(() => parseInputReceipt({ ...receipt, stage: 'read' }))
  for (const stage of ['unknown', 'submitting', 'provider_accepted', 'associated']) assert.throws(() => parseInputReceipt({ ...safe, stage }))
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
  assert.equal(prepareMainSend({ ...input, text: '  ' }).ok, false)
  assert.equal(prepareMainSend({ ...input, text: '中'.repeat(21846) }).ok, false)
  assert.equal(prepareMainSend({ ...input, text: 'a'.repeat(65536) }).ok, true)
})
test('report results default on and survive retries; only the exact admission receipt settles a message', () => {
  const prepared = prepareMainSend({ conversationId: 'chat', text: '后台处理', pending: null, reportResults: true })
  assert.equal(prepared.ok, true)
  if (!prepared.ok) return
  const retry = prepareMainSend({ conversationId: 'chat', text: '后台处理', pending: prepared.pending, reportResults: false })
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
  const job: ManagerJob = { scope: { kind: 'main' }, trigger: 'm', state: 'failed', retryable: true, runtime }
  assert.equal(canRetryJob(job), true)
  for (const changed of [{ ...job, runtime: null }, { ...job, pending_check: true }, { ...job, retryable: false }, { ...job, state: 'reconciling' }, { ...job, runtime: { ...runtime, execution: 'unknown' as const } }, { ...job, runtime: { ...runtime, execution: 'started' as const } }]) assert.equal(canRetryJob(changed), false)
  assert.deepEqual(parseTurnRuntime(runtime), runtime)
  assert.throws(() => parseTurnRuntime({ ...runtime, execution: 'maybe' }))
  const progress = { kind: 'assignment_progress', assignment_id: 'assignment', revision: 1, state: 'accepted' }
  for (const execution of ['not_started', 'started', 'unknown']) {
    const parsed = parseWorkspaceCommand({ ...progress, execution })
    assert.equal(parsed.kind === 'assignment_progress' && parsed.execution, execution)
  }
  assert.equal('execution' in parseWorkspaceCommand(progress), false)
  assert.throws(() => parseWorkspaceCommand({ ...progress, execution: true }))
  assert.throws(() => parseWorkspaceCommand({ ...progress, execution: 'maybe' }))
  assert.match(runtimeLabel(runtime), /默认模型，实际模型未确认/)
  assert.doesNotMatch(runtimeLabel(runtime), /medium/)
  assert.match(runtimeLabel({ ...runtime, provider: 'pi', switched_from: { provider: 'claude', model: 'opus', reasoning_effort: 'high' }, switch_reason: 'quota' }), /由 Claude Code 的 opus 切换/)
})

test('unverified provider authentication remains sendable', () => {
  const [node] = parseNodes({ nodes: [{ device_id: 'mac', name: '我的 Mac', online: true, local: false, revoked: false, capabilities: { manager: 'claude', manager_state: 'unverified', manager_readiness: { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } }, roots: [], relay: false } }] })
  assert.equal(botCanSend(botReach([node])), true)
})

test('重试 sends the message as it was only when the reply provably did nothing', () => {
  assert.equal(retryText('改一下 README', 'missing_key', false), '改一下 README')
  assert.match(retryText('改一下 README', 'missing_key', true), /做过的不要重做/, 'a task it started may have written')
  assert.match(retryText('改一下 README', 'transport', false), /做过的不要重做/, 'it may have broken off midway')
  assert.match(retryText('改一下 README', undefined, false), /做过的不要重做/, 'a stopped reply may have done part')
})

test('a message may be only its files', () => {
  assert.equal(prepareMainSend({ conversationId: 'chat', text: ' ', pending: null }).ok, false)
  const files = prepareMainSend({ conversationId: 'chat', text: ' ', pending: null, files: true })
  assert.equal(files.ok && files.body.text, '')
})

test("a model the bot set itself is changed in its chat, not on the device", async () => {
  const { replyFailure } = await import("../lib/errors.ts")
  assert.equal(replyFailure("no_model", "model not found（这个 bot 设定的模型是 gpt-x）"), "这个 Bot 设定的模型 gpt-x 用不了。在聊天里让它换一个。")
  assert.match(replyFailure("no_model", "model not found"), /设备/)
})


test('new draft defaults become explicit signed booleans, and false survives durable retries', async () => {
  for (const reportResults of [undefined, true, false]) {
    const prepared = prepareMainSend({ conversationId: 'chat', text: '后台工作', reportResults })
    assert.equal(prepared.ok, true)
    if (!prepared.ok) continue
    const expected = reportResults ?? true
    assert.equal(prepared.pending.report_results, expected)
    assert.equal(prepared.body.report_results, expected)
    assert.equal(record(JSON.parse(JSON.stringify(inputEventBody(prepared.body)))).report_results, expected)
    const item: OutboxItem = { key: prepared.pending.message_id, method: 'message', created_at_ms: 1, delivery: 'unknown', params: { ...prepared.body, attestation: { device_id: 'client', signature: 'original-signature' } } }
    const entries = new Map<string, OutboxItem>()
    const store = { all: async () => [...entries.values()], put: async (value: OutboxItem) => { entries.set(value.key, structuredClone(value)) }, delete: async (key: string) => { entries.delete(key) } }
    await enqueue(store, item)
    const restored = parsePendingMessage((await store.all())[0])
    const retried = prepareMainSend({ conversationId: 'chat', text: restored.text, pending: restored, reportResults: !expected })
    assert.equal(retried.ok, true)
    if (!retried.ok) continue
    assert.equal(retried.body.report_results, expected)
    assert.deepEqual(retried.body, prepared.body)
    assert.deepEqual((await store.all())[0], item)
  }
})

test('untyped message, queued input and history boundaries require explicit report_results booleans', () => {
  const body = { message_id: 'input', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, text: '原输入' }
  const queue = (input: unknown) => ({ channels: [], tasks: [], assignments: [], inputs: [{ input, conversation_id: 'chat', source_event: 'source', control_event: 'control', effective_order: 1, deferred: false, cancelled: false }], coverage: { limit: 20, truncated: false } })
  const history = (input: object) => ({ events: [{ event_hash: 'event', created_at_ms: 1, scope_id: 'chat', kind: 'input_posted', body: { type: 'input_posted', ...input }, speaker: { kind: 'human', device_id: 'client' } }], coverage: { limit: 20, truncated: false, scope: 'workspace' } })
  for (const report_results of [undefined, null, 0, 1, 'true', 'false', {}, []]) {
    const input = { ...body, ...(report_results === undefined ? {} : { report_results }) }
    assert.throws(() => parseMessageRequest({ conversation_id: 'chat', ...input }), /report_results/)
    assert.throws(() => parsePendingMessage({ key: 'input', method: 'message', params: { conversation_id: 'chat', ...input, attestation: { device_id: 'client', signature: 'original' } }, created_at_ms: 1, delivery: 'unknown' }), /report_results/)
    assert.throws(() => parseWorkspaceState(queue(input)), /report_results/)
    assert.throws(() => parseWorkspaceHistory(history(input)), /report_results/)
  }
  for (const report_results of [true, false]) {
    const input = { ...body, report_results }
    const parsed = parseMessageRequest({ conversation_id: 'chat', ...input })
    assert.equal(parsed.report_results, report_results)
    assert.deepEqual(inputEventBody(parsed), { type: 'input_posted', ...input })
    assert.equal(parseWorkspaceState(queue(input)).inputs[0].input.report_results, report_results)
    const event = parseWorkspaceHistory(history(input)).events[0]
    assert.equal(event.body.type, 'input_posted')
    if (event.body.type !== 'input_posted') assert.fail('input event changed kind')
    assert.equal(event.body.report_results, report_results)
  }
})
test('chat input steers by default and starts a task only when the prompt asks', () => {
  for (const text of ['新任务：整理发布说明', ' New task: fix login', 'new  task', '新任务帮我修登录', '新任务']) assert.equal(promptIntent(text), 'new_task', text)
  for (const text of ['顺便看看新任务的进度', 'new tasks list', 'renew task', '']) assert.equal(promptIntent(text), 'continue', text)
})
