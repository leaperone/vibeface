import * as deliveryModel from '@vibeface/client/delivery/model.ts'

import assert from "node:assert/strict"
import test from "node:test"

import * as recoveryLinks from '@vibeface/client/identity/recovery-link.ts'
import { entryJson, messageOf, parseEntry, parseInfo, providerNames } from "@vibeface/client/lib/node.ts"

import type { JoinRequest } from "@vibeface/client/lib/node.ts"
import type { InputReceipt, InputTarget, PendingMessage, WorkspaceDomainState } from "@vibeface/client/workspace/types.ts"
import { botReadiness, parseConversation, pendingChecks, parseNodes } from "@vibeface/client/workspace/model.ts"
import { placeChanges, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { awaitsAnswer, foldAnswers, questionOpen, sharedProject } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { addDeviceCopy, deviceCopy, joinCopy, setUpOnManager } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread, taskMessage } from "@vibeface/client/chat/timeline.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"
import { openJoins } from "@vibeface/client/workspace/inbox.ts"

import { cn } from '../../lib/utils.ts'

import { context, historyActions, componentHarness, pendingResult, componentText, findActionProps, conversationHarness, parityBot, parityNode, parityWork, parityShared, settleComponent } from './entry-url-test-kit.test-support.ts'


test("conversation preserves local input while execution devices are unavailable", () => {
  let sent = 0, refreshed = 0
  const workspace = (nodes: unknown[]) => ({
    ...historyActions, generation: 1, isCurrent: () => true, refresh: async () => { refreshed++ }, conversationId: 'chat', fresh: new Set(), data: context, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes, liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: async () => { sent++; return true }, run: async (action: () => Promise<void>) => { await action() },
  })
  let currentWorkspace = workspace([{ device_id: 'gateway', online: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }])
  const app = componentHarness('../chat/conversation-screen.tsx', { '@vibeface/client/workspace/labels.ts': taskLabels, '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const composer = () => app.render('ConversationScreen').find(({ type }) => type === 'PromptInput')?.props
  try {
    composer()?.onChange?.('hello'); composer()?.onSubmit?.()
    assert.equal(composer()?.disabled, false)
    assert.match(String(composer()?.reason), /运行 Bot/)
    assert.equal(sent, 1, 'offline execution does not block local input acceptance')
    assert.ok(refreshed > 0, "a bot no device can reply for reads readiness again")
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'needs_setup', roots: [] } }])
    assert.equal(composer()?.disabled, false, "execution setup does not lock the local composer")
    assert.equal(composer()?.hint, '', "an empty chat offers setup in its middle, not again under the box")
    assert.equal(composer()?.reason, '运行 Bot 的设备需设置', "the composer still explains the execution hold")
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }])
    assert.equal(composer()?.disabled, false)
    composer()?.onChange?.('second input')
    composer()?.onSubmit?.()
    assert.equal(sent, 2)
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'unverified', roots: [] } }])
    assert.equal(composer()?.disabled, false, 'unknown authentication can be tried without claiming readiness')
    assert.equal(composer()?.reason, '')
    assert.doesNotMatch(componentText(composer()?.children), /尚未确认|可用性|Codex/)
    composer()?.onChange?.('third input')
    composer()?.onSubmit?.()
    assert.equal(sent, 3)
    currentWorkspace.canSaveMessages = false
    assert.equal(composer()?.disabled, true, "an unread or revoked identity still prevents signing new input")
    currentWorkspace.canSaveMessages = true
    currentWorkspace.data = { ...context, conversation: { ...context.conversation, owner: 'away' } }
    assert.equal(app.render('ConversationScreen').some(({ props }) => String(props.title).includes('离线，对话在等它')), true, 'an unverified local provider still offers to take over from an offline owner')
  } finally { app.close() }
})


test("an owner's reply on its way keeps its streamed text until the reply lands", () => {
  const job = { scope: { kind: 'main' as const }, trigger: "message:m1", state: "publishing", message_id: "m1", reply_message_id: "r1", source_node: "owner" }
  const ask = { message_id: "m1", event_hash: "m1", role: "user", text: "hi", created_at_ms: 1 }
  const progress = { kind: "manager", key: "live", conversation_id: "chat", trigger: "message:m1", source_node: "owner", sequence: 1, text: "partial answer", items: [], needsRefresh: false }
  let messages: object[] = [ask]
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), data: { ...parseConversation(context, "chat"), messages, scheduler: { scope_jobs: [], jobs: [job], unanswered: [] } }, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [progress], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    refresh: async () => {},
  }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const streamed = () => (app.render('ConversationScreen').find(({ type }) => type === 'Chat')?.props.entries as { message?: { state?: string; text: string } }[]).filter((entry) => entry.message?.state === 'streaming').map((entry) => entry.message?.text).join()
  try {
    assert.equal(streamed(), "partial answer")
    messages = [ask, { message_id: "r1", event_hash: "r1", role: "assistant", text: "full answer", created_at_ms: 2, in_reply_to: "m1" }]
    assert.equal(streamed(), "")
  } finally { app.close() }
})


test("a closed question neither waits on the person nor hides 思考中", () => {
  const job = { scope: { kind: 'main' as const }, trigger: "message:m1", state: "running", message_id: "m1" }
  const ask = { message_id: "m1", event_hash: "m1", role: "user", text: "hi", created_at_ms: 1 }
  const question = (question_id: string, closed: string | null) => ({ question_id, conversation_id: "chat", bot: null, severity: "attention", text: "允许吗？", choices: [],
    about: { node: "gateway", agent: "codex", session_id: "s", cwd: "/w", summary: "", request_id: "r" }, author: "gateway", created_at_ms: 2, answer: null, closed })
  let questions = [question("lost", "request_lost")]
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), data: { ...parseConversation(context, "chat"), messages: [ask], questions, scheduler: { scope_jobs: [], jobs: [job], unanswered: [] } }, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true, answered: {},
    refresh: async () => {},
  }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const thinking = () => (app.render('ConversationScreen').find(({ type }) => type === 'Chat')?.props.entries as { key: string }[]).some((entry) => entry.key === 'activity')
  try {
    assert.equal(thinking(), true, "a question its device closed is not the bot's status")
    questions = [question("open", null)]
    assert.equal(thinking(), false, "an open question is the status already")
  } finally { app.close() }
})


test('offline chat keeps reconnect and stale waiting summary above the composer, then drops the stale prefix when back online', () => {
  let connectionOnline = false
  let retried = 0
  const question = { question_id: 'wait-q', conversation_id: 'chat', bot: null, severity: 'attention' as const, text: '继续吗？', choices: [{ label: '继续' }],
    about: { node: 'gateway', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' }, author: 'gateway', created_at_ms: 1, answer: null, closed: null }
  const workspace = () => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), pendingMessages: [], canSaveMessages: connectionOnline, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', name: '本机', online: true, local: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }],
    liveProgress: [], interrupted: [], writable: connectionOnline, selectedBot: parityBot, connectionOnline, answered: {},
    data: { ...parseConversation(context, 'chat'), questions: [question], scheduler: null, messages: [] },
    refresh: async () => {},
  })
  const harness = conversationHarness(workspace, { retryNow: () => { retried++ } })
  try {
    const above = componentText(harness.composer()?.above)
    assert.match(above, /重试连接/)
    assert.match(above, /最近记录 · /)
    assert.ok(harness.composer()?.disabled)
    const reconnect = harness.findAction('重试连接')?.onClick
    assert.equal(typeof reconnect, 'function')
    if (typeof reconnect === 'function') reconnect()
    assert.equal(retried, 1)
    connectionOnline = true
    assert.doesNotMatch(componentText(harness.composer()?.above), /最近记录 · /)
    assert.match(componentText(harness.composer()?.above), /待处理/)
    assert.equal(harness.composer()?.disabled, false)
  } finally { harness.app.close() }
})


test('merged and in-flight user messages show honest status labels and refresh-only 再查一次', async () => {
  let refreshed = 0, sent = 0
  let connectionOnline = true
  const baseAsk = { message_id: 'm0', event_hash: 'm0', role: 'user' as const, text: 'first', created_at_ms: 0 }
  const build = (jobs: object[], messages: object[], pending: PendingMessage | null = null) => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), pendingMessages: pending ? [pending] : [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', online: connectionOnline, local: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }],
    liveProgress: [], interrupted: [], writable: true, selectedBot: parityBot, connectionOnline,
    data: { ...parseConversation(context, 'chat'), messages, scheduler: { jobs, unanswered: [], capabilities: { streaming: true, cancel: true, steer: true } } },
    refresh: async () => { refreshed++ }, sendMessage: async () => { sent++; return true },
  })
  let current = build([], [], { report_results: true, message_id: 'pending', text: 'sending', conversation_id: 'chat', target: { kind: 'bot_main', bot_id: parityBot.bot_id }, intent: 'continue', sequence: 1, created_at_ms: 1, delivery: 'submitting' })
  const harness = conversationHarness(() => current)
  try {
    assert.match(componentText(harness.messageTree('pending')), /正在确认送达/)
    current = build(
      [{ trigger: 'message:m0', state: 'running', message_id: 'm0' }, { trigger: 'message:m1', state: 'queued', message_id: 'm1' }],
      [baseAsk, { message_id: 'm1', event_hash: 'm1', role: 'user', text: 'queued', created_at_ms: 1 }],
    )
    assert.match(componentText(harness.messageTree('m1')), /已排队/)
    for (const [outcome, label] of [['answered', '关联回合已完成'], ['unfinished', '关联回合未完成'], ['unknown', '补充结果未知']] as const) {
      current = build([{ trigger: 'message:m2', state: 'steered', message_id: 'm2', outcome }], [{ message_id: 'm2', event_hash: 'm2', role: 'user', text: outcome, created_at_ms: 2 }])
      const tree = harness.messageTree('m2')
      assert.ok(componentText(tree).includes(label))
      if (outcome === 'unknown') {
        refreshed = 0; sent = 0
        const action = findActionProps(tree, '再查一次')
        assert.ok(action?.action)
        await (action.action as () => Promise<void>)()
        assert.equal(refreshed, 1)
        assert.equal(sent, 0)
        connectionOnline = false
        current = { ...current, connectionOnline }
        assert.equal(findActionProps(harness.messageTree('m2'), '再查一次')?.disabled, true)
      }
    }
  } finally { harness.app.close() }
})


test('queued input edits and cancellation use the exact input CAS and leave the chat draft alone', async () => {
  const target: InputTarget = { kind: 'bot_main', bot_id: parityBot.bot_id }
  const queued: WorkspaceDomainState['inputs'][number] = { input: { report_results: true, message_id: 'queued', target, intent: 'defer', sequence: 1, text: 'original queued text' }, conversation_id: 'chat', source_event: 'source', control_event: 'control', effective_order: 1, deferred: true, cancelled: false }
  const controls: unknown[] = [], queries: unknown[] = []
  let stage: InputReceipt['stage'] = 'entry_accepted', error: unknown = null
  const shared = { ...parityShared(), workspaceState: { channels: [], tasks: [], assignments: [], inputs: [queued], coverage: { limit: 128, truncated: false } },
    queryInput: async (request: unknown) => { queries.push(JSON.parse(JSON.stringify(request))); return { stage } },
    controlInput: async (request: unknown) => { controls.push(JSON.parse(JSON.stringify(request))) },
    readWorkspaceState: async () => {},
    cancelManager: async () => { throw new Error('an input cannot cancel the manager job') },
    sendMessage: async () => { throw new Error('editing must not send a replacement message') },
  }
  const app = componentHarness('./workspace-chat.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
    '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error, run: async (_key: string, action: () => Promise<void>) => { try { await action() } catch (cause) { error = cause } } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
  })
  const render = () => app.render('InputQueue', { target })
  const press = async (label: string) => { const button = render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label); assert.ok(button && typeof button.props.onClick === 'function'); button.props.onClick(); await settleComponent() }
  try {
    await press('编辑输入')
    const edit = render().find(({ type }) => type === 'Textarea')
    assert.ok(edit && typeof edit.props.onChange === 'function')
    edit.props.onChange({ currentTarget: { value: 'edited queued text' } })
    await press('保存输入修改')
    assert.deepEqual(queries, [{ message_id: 'queued', target, conversation_id: 'chat' }])
    assert.deepEqual(controls, [{ message_id: 'queued', expected_event: 'control', action: { kind: 'edit', text: 'edited queued text', attachments: [] } }])
    await press('取消稍后输入')
    assert.deepEqual(controls.at(-1), { message_id: 'queued', expected_event: 'control', action: { kind: 'cancel' } })
    stage = 'unknown'
    await press('取消稍后输入')
    assert.equal(controls.length, 2, 'a changed receipt cannot authorize another control write')
    assert.match(String(error), /可能已提交/)
  } finally { app.close() }
})


test('task detail keeps long progress collapsed by default and finished tasks keep their result over stale progress', async () => {
  const work = parityWork({ state: 'finished', outcome: 'completed', result_text: '最终结果', summary: '✓ 读记录\n→ 核对结果\n· 等待同步' })
  const workspace = {
    ...parityShared(), readWork: async () => ({ work: { ...work, conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } }),
    generation: 1, connectionOnline: true, writable: true, incomplete: false, selectedBot: parityBot, nodes: [parityNode], answered: {},
    data: parseConversation({ ...context, works: [parityWork({ state: 'finished', outcome: 'completed', result_text: '最终结果', summary: '✓ 读记录\n→ 核对结果\n· 等待同步' })] }),
    liveProgress: [{ kind: 'native', work_id: 'work', target_node: 'worker', text: '旧进展行', sequence: 1 }],
  }
  const app = componentHarness('./task-detail.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_kind: string, action: () => Promise<void>) => action() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/lib/errors.ts': errors,
    '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/lib/node.ts': { providerNames },
  }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  try {
    app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    await settleComponent()
    const elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.equal(elements.some(({ props }) => props.text === '最终结果'), true)
    assert.doesNotMatch(componentText(elements), /旧进展行/)
    const steps = elements.find(({ props }) => props.label === '最近步骤摘要 · 3')
    assert.equal(steps?.props.open, false)
    assert.equal(elements.some(({ props }) => props.label === '详细进展'), false)
  } finally { app.close() }
})


test('unknown task detail reads its exact work without starting a Main request and keeps manual settlement available', async () => {
  let navigated = false, sent = 0, refreshed = 0
  const reads: unknown[] = []
  let connectionOnline = true
  const work = parityWork({ state: 'unverified', outcome: 'unverified', failure_reason: '无法证明结果' })
  const workspace = {
    ...parityShared(),
    readWork: async (input: unknown) => { reads.push(JSON.parse(JSON.stringify(input))); return { work: { ...work, conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } } },
    refresh: async () => { refreshed++ },
    ...historyActions, generation: 1, isCurrent: () => true, connectionOnline, writable: true, incomplete: false, selectedBot: parityBot, nodes: [parityNode], answered: {},
    data: parseConversation({ ...context, works: [work] }),
    liveProgress: [],
    sendMessage: async () => { sent++; return false },
    resolveCheck: async () => {},
  }
  const app = componentHarness('./task-detail.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => { navigated = true }, Link: 'Link' },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_kind: string, action: () => Promise<void>) => action() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/lib/errors.ts': errors,
    '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/lib/node.ts': { providerNames },
    './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
  }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  try {
    app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    await settleComponent()
    let elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.match(componentText(elements), /还不知道做完没有/)
    const ask = findActionProps(elements, '先查结果')
    assert.ok(ask?.action)
    assert.equal(ask.disabled, false)
    await (ask.action as () => Promise<void>)()
    assert.equal(sent, 0)
    assert.equal(refreshed, 1)
    assert.equal(navigated, false)
    app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    await settleComponent()
    assert.deepEqual(reads, [{ conversation_id: 'chat', work_id: 'work' }, { conversation_id: 'chat', work_id: 'work' }])
    workspace.connectionOnline = false
    elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.equal(findActionProps(elements, '先查结果')?.disabled, true)
    assert.equal(elements.some(({ props }) => props.label === '我已自行核对'), true)
  } finally { app.close() }
})


test("diagnostics and connection ID preserve their complete payload, labels and scoped two-second feedback", async () => {
  for (const name of ['CopyDiagnostics', 'ConnectionId']) {
    const writes: string[] = [], timers: { run(): void; duration: number }[] = []
    let pending = pendingResult()
    const app = componentHarness(name === 'CopyDiagnostics' ? '../identity/device-controls.tsx' : '../lab/transfers.tsx', {
      '@vibeface/client/delivery/model.ts': deliveryModel,
    }, { navigator: { clipboard: { writeText: (value: string) => { writes.push(value); return pending.promise } } }, setTimeout: (run: () => void, duration: number) => { timers.push({ run, duration }); return timers.length }, clearTimeout: () => {} }, name === 'ConnectionId' ? 'ConnectionId' : '')
    const render = (next = false) => app.render(name, name === 'CopyDiagnostics' ? { lines: [['个人网络', next ? 'network-two' : 'network-one'], ['空值', ''], ['未提供', null], ['授权版本', 0]] } : { value: next ? 'b'.repeat(64) : 'a'.repeat(64) })
    try {
      const button = render().find(({ type }) => type === 'Button')?.props
      assert.ok(button && typeof button.onClick === 'function')
      assert.equal(componentText(button.children), name === 'CopyDiagnostics' ? '复制诊断信息' : '复制')
      button.onClick()
      assert.deepEqual(writes, [name === 'CopyDiagnostics' ? '个人网络: network-one\n授权版本: 0' : 'a'.repeat(64)])
      pending.resolve(undefined); await settleComponent()
      const copied = render()
      assert.ok(copied.some(({ props }) => componentText(props.children) === '已复制'))
      if (name === 'CopyDiagnostics') assert.ok(copied.some(({ props }) => props.role === 'status' && componentText(props.children) === '已复制'))
      assert.equal(timers[0].duration, 2000)
      assert.equal(render(true).some(({ props }) => componentText(props.children) === '已复制'), false)
      pending = pendingResult()
      const retry = render(true).find(({ type }) => type === 'Button')?.props.onClick
      assert.ok(typeof retry === 'function'); retry()
      pending.reject(new Error('refused')); await settleComponent()
      const failed = render(true)
      assert.equal(failed.some(({ props }) => props.error === '复制失败，请重试。'), name === 'CopyDiagnostics')
      assert.equal(failed.some(({ props }) => componentText(props.children) === '已复制'), false)
    } finally { app.close() }
  }
})


test("relay copy remains persistent and old connections cannot publish success", async () => {
  let generation = 1, pending = pendingResult()
  const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, request: async () => ({ link: `vibeface://node-${owner}` }) } }
  const device = { ready: ready() }
  const app = componentHarness('../devices/relay-settings.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    '@vibeface/client/lib/node.ts': { parseShareLink: (raw: { link: string }) => raw.link },
  }, { navigator: { clipboard: { writeText: () => pending.promise } }, setTimeout: () => assert.fail('relay success must not expire'), clearTimeout: () => {} })
  const render = () => app.render('ShareLink')
  const copy = () => { const action = render().find(({ type }) => type === 'Button')?.props.onClick; assert.ok(typeof action === 'function'); action() }
  try {
    render(); await settleComponent(); copy()
    pending.resolve(undefined); await settleComponent()
    assert.ok(render().some(({ props }) => props['aria-label'] === '已复制'))
    pending = pendingResult(); copy()
    pending.reject(new Error('refused')); await settleComponent()
    assert.ok(render().some(({ props }) => props['aria-label'] === '已复制'))
    pending = pendingResult(); copy()
    generation++; device.ready = ready(); render(); await settleComponent()
    pending.resolve(undefined); await settleComponent()
    assert.equal(render().some(({ props }) => props['aria-label'] === '已复制'), false)
  } finally { app.close() }
})


test("single-record copying keeps toast-only feedback, handles refusal and ignores a replaced record", async () => {
  let pending = pendingResult()
  const writes: string[] = [], notices: unknown[][] = []
  const record = { ts_ms: 0, source: 'web', level: 'info', event: 'test', message: 'complete record' }
  const app = componentHarness('../../routes/_app/settings_.logs.tsx', {
    '@tanstack/react-router': { createFileRoute: () => () => ({}) },
    '@/lib/glossary': { terms: { thisComputer: { label: '本机', icon: 'Cpu' } } },
    '@/lib/utils': { cn },
    '@/components/arc/product-kit': { Details: 'Details', toast: (...args: unknown[]) => notices.push(args) },
    '@vibeface/client/lib/errors.ts': { knownError: () => null },
  }, { navigator: { clipboard: { writeText: (value: string) => { writes.push(value); return pending.promise } } }, setTimeout: () => assert.fail('record feedback must stay toast-only'), clearTimeout: () => {} }, 'LogRow')
  const render = (next = record) => app.render('LogRow', { record: next, name: () => '', open: true, onToggle() {}, onCopyAll: async () => {} })
  const click = () => { const action = render().find(({ type, props }) => type === 'Button' && componentText(props.children) === '复制这条')?.props.onClick; assert.ok(typeof action === 'function'); action() }
  try {
    click()
    assert.deepEqual(writes, ['{"ts_ms":0,"source":"web","level":"info","event":"test","message":"complete record"}'])
    assert.equal(notices.length, 0)
    pending.resolve(undefined); await settleComponent()
    assert.equal(notices.length, 1)
    assert.equal(notices[0].length, 2)
    assert.equal(notices[0][0], '已复制这条记录')
    const successOptions = notices[0][1]
    assert.ok(successOptions && typeof successOptions === 'object' && 'tone' in successOptions)
    assert.equal(successOptions.tone, 'success')
    assert.equal(Object.keys(successOptions).join(','), 'tone')
    pending = pendingResult(); click()
    pending.reject(new Error('private platform details')); await settleComponent()
    assert.equal(notices.length, 2)
    assert.equal(notices[1].length, 2)
    assert.equal(notices[1][0], '复制失败，请重试。')
    const failureOptions = notices[1][1]
    assert.ok(failureOptions && typeof failureOptions === 'object' && 'tone' in failureOptions)
    assert.equal(failureOptions.tone, 'warning')
    assert.equal(Object.keys(failureOptions).join(','), 'tone')
    pending = pendingResult(); click()
    render({ ...record, message: 'replacement' })
    pending.resolve(undefined); await settleComponent()
    assert.equal(notices.length, 2)
  } finally { app.close() }
})


test("an outstanding clipboard permission cannot keep the add-device sheet busy or release another action", async () => {
  let generation = 1, creation = pendingResult()
  const invitation = { link: 'vibeface://join/vibeface1test', inviteId: 'a'.repeat(32), expiresAt: Date.now() + 600_000 }
  const clipboard = pendingResult()
  const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, principal: { can_manage: true, network_id: 'network' }, entry: { kind: 'gateway', address: 'https://node.example', device_id: 'target' } } }
  const device = { connectionGeneration: () => generation, ready: ready(), joins: [], createInvitation: () => creation.promise, refreshDirectory: async () => {} }
  const app = componentHarness('../devices/add-device.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { label: '安全码', icon: 'ScanEye' } } },
    '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message },
  }, { navigator: { clipboard: { writeText: () => clipboard.promise } }, setInterval: () => 0, clearInterval: () => {} })
  const render = () => app.render('AddDevice')
  const labelled = (label: string) => render().find(({ props }) => [props.children].flat().includes(label))?.props
  try {
    render()
    labelled('生成链接')?.onClick?.(); creation.resolve(invitation); await settleComponent()
    labelled('复制链接')?.onClick?.()
    assert.equal(labelled('复制链接')?.disabled, true)
    generation++; device.ready = ready(); render(); render()
    assert.equal(labelled('生成链接')?.disabled, false)
    creation = pendingResult(); labelled('生成链接')?.onClick?.()
    clipboard.resolve(undefined); await settleComponent()
    assert.equal(labelled('生成链接')?.disabled, true)
    creation.resolve(invitation); await settleComponent()
    assert.equal(labelled('复制链接')?.disabled, false)
  } finally { app.close() }
})


test("add-device clipboard refusal selects the full attempted command and a busy click cannot change it", async () => {
  let pending = pendingResult()
  const copied: string[] = [], focus: string[] = []
  const link = 'vibeface://join/full-invitation'
  const command = "vibeface join 'vibeface://join/full-invitation' --name '我的电脑'"
  const device = { connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true } }, createInvitation: async () => ({ link, inviteId: 'a'.repeat(32), expiresAt: Date.now() + 600_000 }), refreshDirectory: async () => {} }
  const app = componentHarness('../devices/add-device.tsx', {
    '@vibeface/client/identity/copy.ts': { addDeviceCopy, deviceCopy, joinCopy },
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { icon: 'ScanEye' } } },
    '@vibeface/client/lib/node.ts': { messageOf },
  }, { navigator: { clipboard: { writeText: (value: string) => { copied.push(value); return pending.promise } } }, setInterval: () => 0, clearInterval: () => {} })
  const render = () => app.render('AddDevice')
  const click = (label: string) => { const action = render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props.onClick; assert.ok(typeof action === 'function'); action() }
  try {
    click('生成链接'); await settleComponent()
    const kind = render().find(({ type }) => type === 'Segmented')?.props.onChange
    assert.ok(typeof kind === 'function'); kind('computer')
    const raw = render().find(({ props }) => props.readOnly)?.props.ref
    assert.ok(raw && typeof raw === 'object')
    Reflect.set(raw, 'current', { focus: () => focus.push('focus'), select: () => focus.push('select') })
    click('复制命令')
    assert.deepEqual(copied, [command], 'the write starts before the click handler returns')
    click('复制链接')
    assert.deepEqual(copied, [command], 'a busy action cannot start another write')
    assert.equal(render().find(({ props }) => props.readOnly)?.props.value, command)
    pending.reject(new Error('clipboard denied')); await settleComponent()
    const tree = render()
    assert.ok(tree.some(({ props }) => props.role === 'alert' && componentText(props.children) === '没能复制，手动选中下面的内容'))
    assert.equal(tree.find(({ props }) => props.label === '链接原文' && 'open' in props)?.props.open, true)
    assert.equal(tree.find(({ props }) => props.readOnly)?.props.value, command)
    assert.deepEqual(focus, ['focus', 'select'])
    assert.equal(tree.some(({ props }) => componentText(props.children) === '已复制'), false)
    pending = pendingResult(); click('复制链接')
    assert.equal(render().find(({ props }) => props.readOnly)?.props.value, link)
    assert.equal(render().some(({ props }) => props.role === 'alert'), false)
    pending.reject(new Error('refused again')); await settleComponent(); render()
    assert.deepEqual(copied, [command, link])
    assert.deepEqual(focus, ['focus', 'select', 'focus', 'select'])
  } finally { app.close() }
})


test("add-device copy cannot publish or select after kind, invitation, drawer or connection changes", async () => {
  for (const change of ['kind', 'invitation', 'drawer', 'connection']) for (const outcome of ['resolve', 'reject']) {
    let generation = 1, calls = 0
    const pending = pendingResult(), focus: string[] = []
    const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, principal: { can_manage: true } } }
    const device = { connectionGeneration: () => generation, ready: ready(), createInvitation: async () => ({ link: `vibeface://join/invitation-${++calls}`, inviteId: calls.toString(16).padStart(32, '0'), expiresAt: Date.now() + 600_000 }), refreshDirectory: async () => {} }
    const app = componentHarness('../devices/add-device.tsx', {
      '@vibeface/client/identity/copy.ts': { addDeviceCopy, deviceCopy, joinCopy },
      '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { icon: 'ScanEye' } } }, '@vibeface/client/lib/node.ts': { messageOf },
    }, { navigator: { clipboard: { writeText: () => pending.promise } }, setInterval: () => 0, clearInterval: () => {} })
    const render = () => app.render('AddDevice')
    const click = (label: string) => { const action = render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props.onClick; assert.ok(typeof action === 'function'); action() }
    try {
      click('生成链接'); await settleComponent()
      const raw = render().find(({ props }) => props.readOnly)?.props.ref
      assert.ok(raw && typeof raw === 'object')
      Reflect.set(raw, 'current', { focus: () => focus.push('focus'), select: () => focus.push('select') })
      click('复制链接')
      if (change === 'kind') { const action = render().find(({ type }) => type === 'Segmented')?.props.onChange; assert.ok(typeof action === 'function'); action('computer') }
      if (change === 'drawer') { const action = render().find(({ type }) => type === 'Drawer')?.props.onOpenChange; assert.ok(typeof action === 'function'); action(false) }
      if (change === 'connection' || change === 'invitation') { generation++; device.ready = ready(); render(); render() }
      if (change === 'invitation') { click('生成链接'); await settleComponent() }
      render()
      if (outcome === 'resolve') pending.resolve(undefined)
      else pending.reject(new Error('old refusal'))
      await settleComponent()
      const tree = render()
      assert.equal(tree.some(({ props }) => props.role === 'alert' && componentText(props.children) === '没能复制，手动选中下面的内容'), false, `${change}/${outcome}`)
      assert.equal(tree.some(({ props }) => componentText(props.children) === '已复制'), false, `${change}/${outcome}`)
      assert.equal(focus.length, 0, `${change}/${outcome}`)
      if (change === 'invitation') assert.equal(tree.find(({ props }) => props.readOnly)?.props.value, 'vibeface://join/invitation-2')
    } finally { app.close() }
  }
})


test("the invitation's QR code is its link, for the app", async () => {
  const shown = async (link: string) => {
    const encoded: string[] = []
    const device = { connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true, network_id: 'network' }, entry: { kind: 'gateway', address: 'http://127.0.0.1:4790', device_id: 'target' } }, joins: [], createInvitation: async () => ({ link, inviteId: 'a'.repeat(32), expiresAt: Date.now() + 600_000 }), refreshDirectory: async () => {} }
    const app = componentHarness('../devices/add-device.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { label: '安全码', icon: 'ScanEye' } } },
      '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message }, 'qrcode-generator': () => ({ addData(value: string) { encoded.push(value) }, make() {}, createDataURL: () => 'data:qr' }),
    }, { setInterval: () => 0, clearInterval: () => {} })
    try {
      app.render('AddDevice'); app.render('AddDevice').find(({ props }) => [props.children].flat().includes('生成链接'))?.props.onClick?.(); await settleComponent()
      return { tree: app.render('AddDevice'), encoded }
    } finally { app.close() }
  }
  const local = await shown('vibeface://join/vibeface1test')
  assert.deepEqual(local.encoded, ['vibeface://join/vibeface1test'])
  assert.equal(local.tree.find(({ type }) => type === 'img')?.props.alt, '用手机 App 扫码')
  assert.ok(local.tree.some(({ type, props }) => type === 'p' && [props.children].flat().includes('用手机 App 扫码')))
})


test("an oversized invitation keeps its full link visible and copyable when QR encoding fails", async () => {
  const link = `vibeface://join/${'a'.repeat(5000)}`
  const copied: string[] = []
  const device = { connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true } }, createInvitation: async () => ({ link, inviteId: 'a'.repeat(32), expiresAt: Date.now() + 600_000 }), refreshDirectory: async () => {} }
  const app = componentHarness('../devices/add-device.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { icon: 'ScanEye' } } },
    '@vibeface/client/lib/node.ts': { messageOf },
  }, { navigator: { clipboard: { writeText: async (value: string) => { copied.push(value) } } }, setInterval: () => 0, clearInterval: () => {} })
  try {
    app.render('AddDevice')
    app.render('AddDevice').find(({ props }) => [props.children].flat().includes('生成链接'))?.props.onClick?.()
    await settleComponent()
    const tree = app.render('AddDevice')
    assert.equal(tree.some(({ type }) => type === 'img'), false)
    assert.ok(tree.some(({ props }) => props.error === '无法生成二维码，请复制链接。'))
    assert.equal(tree.find(({ props }) => props.label === '链接原文' && 'open' in props)?.props.open, true)
    assert.equal(tree.find(({ props }) => props.readOnly)?.props.value, link)
    const copy = tree.find(({ props }) => [props.children].flat().includes('复制链接'))?.props
    assert.ok(copy?.onClick)
    copy.onClick()
    await settleComponent()
    assert.deepEqual(copied, [link])
  } finally { app.close() }
})


test('invitation expiry and consumption are scoped to the issued link on Web and Mobile', async () => {
  for (const platform of ['web', 'native']) {
    const web = platform === 'web'
    let now = 1_000, creations = 0, timerId = 0
    const timers = new Map<number, { run: () => void; delay: number }>()
    const joins: { invite_id: string }[] = []
    const writes: string[] = [], shares: string[] = []
    const copy = pendingResult()
    const copyText = (text: string) => { writes.push(text); return copy.promise }
    const invite = (number: number) => ({ link: `vibeface://join/link-${number}`, inviteId: number.toString(16).padStart(32, '0') })
    const device = {
      connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true } },
      createInvitation: async () => ({ ...invite(++creations), expiresAt: now + 600_000 }), refreshDirectory: async () => {},
    }
    const joinModule = { JoinRequests: 'JoinRequests', useOpenJoins: () => joins }
    const app = componentHarness(web ? '../devices/add-device.tsx' : '../../../../../apps/mobile/app/add-device.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device },
      '@vibeface/client/identity/copy.ts': { addDeviceCopy, deviceCopy, joinCopy },
      '@vibeface/client/lib/node.ts': { messageOf },
      '@/lib/glossary': { terms: { safetyCode: { icon: 'ScanEye' } } },
      './join': joinModule, '@/features/devices/join-requests': joinModule,
      'react-native': { View: 'View', ScrollView: 'ScrollView', Share: { share: async ({ message }: { message: string }) => { shares.push(message) } }, AccessibilityInfo: { announceForAccessibility() {} } },
      'expo-clipboard': { setStringAsync: copyText },
    }, {
      Date: { now: () => now }, navigator: { clipboard: { writeText: copyText } },
      setTimeout: (run: () => void, delay: number) => { timers.set(++timerId, { run, delay }); return timerId },
      clearTimeout: (id: number) => timers.delete(id),
      setInterval: () => 0, clearInterval: () => {},
    }, '', { '@/lib/use-run': '../../../../../apps/mobile/src/lib/use-run.ts' })
    const render = () => app.render(web ? 'AddDevice' : 'default')
    const button = (label: string) => render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props
    const press = (label: string) => {
      const action = button(label)?.[web ? 'onClick' : 'onPress']
      assert.ok(typeof action === 'function', `${platform}: ${label}`)
      action()
    }
    const allow = (inviteId: string) => {
      const callback = render().find(({ type }) => type === 'JoinRequests')?.props.onAllowed
      assert.ok(typeof callback === 'function')
      callback({ id: 'joined', kind: 'client', name: '手机', manage: false }, inviteId)
    }
    const showDrawer = (open: boolean) => {
      const change = render().find(({ type }) => type === 'Drawer')?.props.onOpenChange
      assert.ok(typeof change === 'function')
      change(open)
    }
    try {
      render()
      if (web) { showDrawer(true); press('生成链接') }
      await settleComponent()
      assert.equal(render().find(({ type }) => type === 'JoinRequests')?.props.inviteId, invite(1).inviteId)
      joins.push({ invite_id: invite(2).inviteId })
      assert.ok(button('复制链接'), 'an unrelated request leaves the invitation visible')
      joins.length = 0
      allow(invite(2).inviteId)
      assert.ok(button('复制链接'), 'approving a different invite leaves the invitation visible')
      press('复制链接')
      assert.deepEqual(writes, [invite(1).link])
      const staleShare = button('分享')?.onPress
      const firstTimer = timers.get(timerId)
      assert.equal(firstTimer?.delay, 600_000)
      if (web) {
        showDrawer(false); render()
        assert.equal(timers.size, 0)
        showDrawer(true); render()
      }
      now = 601_000
      timers.get(timerId)?.run()
      assert.equal(button('复制链接'), undefined)
      assert.equal(button('分享'), undefined)
      if (typeof staleShare === 'function') staleShare()
      assert.deepEqual(shares, [])
      assert.equal(creations, 1, 'expiry itself does not create another link')
      assert.equal(button('重新生成链接')?.disabled, false)
      press('重新生成链接'); await settleComponent(); render()
      firstTimer?.run()
      copy.resolve(undefined); await settleComponent()
      assert.ok(button('复制链接'), 'old timer and clipboard completion leave the refreshed link visible')
      assert.equal(button('已复制'), undefined)
      joins.push({ invite_id: invite(2).inviteId })
      assert.equal(button('复制链接'), undefined)
      joins.length = 0
      assert.equal(button('复制链接'), undefined, 'the matching invite stays spent when its request leaves')
    } finally { app.close() }
    assert.equal(timers.size, 0)
  }
})


test('only the matching invitation auto-starts and a failed comparison can be started manually on Web and Mobile', async () => {
  for (const platform of ['web', 'native']) {
    const web = platform === 'web'
    const request = (digit: string): JoinRequest => ({
      join_id: digit.repeat(32), invite_id: digit.repeat(32), status: 'pending', kind: 'client', name: '手机', network_id: 'a'.repeat(64),
      device_id: digit.repeat(64), commit: 'd'.repeat(64), admin_id: null, admin_nonce: null, joiner_nonce: null, reason: null, expires_at_ms: Date.now() + 600_000,
    })
    const matching = request('1'), unrelated = request('2'), sent: string[] = []
    const first = pendingResult()
    const device = {
      ready: { principal: { can_manage: true } }, joins: [unrelated, matching],
      startApproval: async (id: string) => { sent.push(id); if (sent.length === 1) await first.promise },
    }
    let busy: string | null = null, error: unknown = null
    const run = async (key: string, work: () => Promise<void>) => {
      busy = key; error = null
      try { await work() } catch (cause) { error = cause } finally { busy = null }
    }
    const source = web ? '../devices/join.tsx' : '../../../../../apps/mobile/src/features/devices/join-requests.tsx'
    const dependencies = {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device },
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@vibeface/client/workspace/inbox.ts': { openJoins }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@/lib/glossary': { terms: { admin: { label: '管理设备' } } },
      '@/lib/use-run': { useRun: () => ({ busy, error, run }) },
      './devices': { deviceIcon: () => 'Icon' }, './device-icon': { deviceIcon: () => 'Icon' },
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy, error, run }) }, { get: (target, name) => name === 'useAction' ? target.useAction : String(name) }),
    }
    const globals = { setInterval: () => 0, clearInterval: () => {} }
    const list = componentHarness(source, dependencies, globals)
    const card = componentHarness(source, dependencies, globals, web ? '' : 'JoinCard')
    try {
      const elements = list.render('JoinRequests', { inviteId: matching.invite_id, manage: true })
      const own = elements.find(({ type, props }) => typeof type === 'function' && type.name === 'JoinCard' && props.request === matching)?.props
      const other = elements.find(({ type, props }) => typeof type === 'function' && type.name === 'JoinCard' && props.request === unrelated)?.props
      assert.equal(own?.auto, true)
      assert.equal(other?.auto, false)
      if (web) { assert.equal(own?.manage, true); assert.equal(other?.manage, false) }
      const render = () => card.render('JoinCard', own)
      const button = (label: string) => render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props
      render()
      assert.equal(button('处理中')?.disabled, true)
      assert.deepEqual(sent, [matching.join_id])
      first.reject(new Error('response lost')); await settleComponent()
      assert.equal(button('显示安全码')?.disabled, false)
      assert.deepEqual(sent, [matching.join_id])
      const retry = button('显示安全码')?.[web ? 'onClick' : 'onPress']
      assert.ok(typeof retry === 'function')
      retry()
      assert.deepEqual(sent, [matching.join_id, matching.join_id])
      await settleComponent()
    } finally { list.close(); card.close() }
  }
})


test('a Web manager invitation cannot carry its management preset into the next link', async () => {
  let issued = 0
  const joins: { invite_id: string }[] = []
  const device = {
    connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true } },
    createInvitation: async () => ({ link: `vibeface://join/link-${++issued}`, inviteId: String(issued).repeat(32), expiresAt: Date.now() + 600_000 }),
    refreshDirectory: async () => {},
  }
  const app = componentHarness('../devices/add-device.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    '@vibeface/client/identity/copy.ts': { addDeviceCopy, deviceCopy, joinCopy },
    '@/lib/glossary': { terms: { safetyCode: { icon: 'ScanEye' } } },
    './join': { JoinRequests: 'JoinRequests', useOpenJoins: () => joins },
  }, { HTMLElement: class {}, document: { activeElement: null }, setTimeout, clearTimeout, setInterval: () => 0, clearInterval() {} })
  const render = () => app.render('AddDevice')
  try {
    app.render('AddDevice', { opening: 'phone', manage: true }); await settleComponent()
    joins.push({ invite_id: '1'.repeat(32) }, { invite_id: 'a'.repeat(32) })
    const own = render().find(({ type }) => type === 'JoinRequests')?.props
    assert.ok(own && typeof own.onAllowed === 'function')
    assert.equal(own.manage, true)
    joins.shift()
    own.onAllowed({ id: 'joined', kind: 'client', name: '手机', manage: true }, '1'.repeat(32))
    const generate = render().find(({ type, props }) => type === 'Button' && componentText(props.children) === '生成链接')?.props.onClick
    assert.ok(generate)
    generate(); await settleComponent()
    const next = render().find(({ type }) => type === 'JoinRequests')?.props
    assert.equal(next?.inviteId, '2'.repeat(32))
    assert.equal(next?.manage, false)
  } finally { app.close() }
})


test("an oversized phone recovery link stays visible and copyable when QR encoding fails", async () => {
  const address = `https://node.example/${'a'.repeat(5000)}`
  const link = `vibeface://join?recover=target&address=https%3A%2F%2Fnode.example%2F${'a'.repeat(5000)}`
  const copied: string[] = []
  const app = componentHarness('../identity/recovery-controls.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ entrances: [], ready: { entry: { kind: 'gateway', address, device_id: 'target' } } }) },
    '@vibeface/client/identity/recovery-link.ts': recoveryLinks,
  }, { navigator: { clipboard: { writeText: async (value: string) => { copied.push(value) } } }, setTimeout, clearTimeout })
  try {
    const tree = app.render('PhoneRecovery')
    assert.equal(tree.some(({ type }) => type === 'img'), false)
    assert.ok(tree.some(({ props }) => props.error === '无法生成二维码，请复制链接。'))
    assert.equal(tree.find(({ props }) => props.readOnly)?.props.value, link)
    const copy = tree.find(({ props }) => [props.children].flat().includes('复制链接'))?.props
    assert.ok(copy?.onClick)
    copy.onClick()
    await settleComponent()
    assert.deepEqual(copied, [link])
    assert.match(componentText(app.render('PhoneRecovery')), /已复制/)
  } finally { app.close() }
})


test("an entry is stored in the one form Nodes send it in, and a LAN entry as its device id and addresses", () => {
  const id = "ab".repeat(32)
  for (const value of [{ address: "https://node.example", device_id: id }, { device_id: id, addrs: ["192.168.1.4:7001", "[fd00::4]:7001"] }]) assert.deepEqual(entryJson(parseEntry(value)), value)
  assert.deepEqual(parseEntry({ device_id: id, addrs: ["192.168.1.4:7001", "node.example:7001"] }), { kind: "lan", addrs: ["192.168.1.4:7001"], device_id: id })
  assert.throws(() => parseEntry({ device_id: id, addrs: ["node.example:7001"] }), /访问地址缺失或格式无效/)
})


test("device id boundaries require the device_id field", () => {
  const id = "ab".repeat(32), other = "cd".repeat(32)
  const parsers = { parseEntry, parseInfo }
  assert.equal(parsers.parseEntry({ address: "https://node.example", device_id: id }).device_id, id)
  for (const relay of [false, true]) {
    const entry = parsers.parseEntry({ address: "https://node.example", device_id: id, relay })
    assert.equal(entry.device_id, id)
    assert.equal(Object.hasOwn(entry, "relay"), false)
  }
  assert.equal(parsers.parseInfo({ name: "node", version: "1", initialized: true, network_id: other, device_id: id }).device_id, id)
  const guest = parsers.parseInfo({ name: "Node", version: "1", initialized: true, network_id: null, device_id: id })
  assert.equal(guest.network_id, null)
  assert.equal(guest.connection_entry, undefined)
  assert.equal(parseNodes({ nodes: [{ local: true, online: true, revoked: false, device_id: id }] })[0].device_id, id)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", node_id: id }), /设备地址无效/)
  assert.throws(() => parsers.parseInfo({ name: "node", version: "1", initialized: true, network_id: other, node_id: id }), /设备地址无效/)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", device_id: "invalid" }), /设备地址无效/)
  assert.throws(() => parseNodes({ nodes: [{ local: true, online: true, revoked: false, node_id: id }] }), /缺少文字/)
})


test('a missing native or web recovery copy never enables external-save confirmation', () => {
  for (const [path, name, saveProp] of [
    ['../../../../../apps/mobile/src/features/recovery/recovery.tsx', 'SaveRecoveryFile', 'save'],
    ['../identity/recovery-controls.tsx', 'SaveRecoveryFile', 'download'],
  ]) {
    const app = componentHarness(path, {
      '@vibeface/client/identity/copy.ts': { recoveryCopy: { why: '', onlyCopy: '', keptApart: '文件和密码分开保存' } },
      '@vibeface/client/lib/secure.ts': { recoveryFileStem: (code: string) => code },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@/components/arc/product-kit': { claimFocus() {}, InlineError: 'InlineError' },
    }, {}, path.includes('/mobile/') ? 'SaveRecoveryFile' : '')
    try {
      const missing = app.render(name, { code: 'ABCDEF12', saved: false, onSaved() {} })
      const confirmation = missing.find(({ props }) => props.type === 'checkbox' || props['aria-label'] === '文件和密码分开保存' || props.accessibilityLabel === '文件和密码分开保存')
      assert.equal(confirmation?.props.disabled, true)
      const handed = app.render(name, { code: 'ABCDEF12', saved: false, onSaved() {}, [saveProp]: { done: true, run() {} } })
      assert.equal(handed.find(({ props }) => props.type === 'checkbox' || props['aria-label'] === '文件和密码分开保存' || props.accessibilityLabel === '文件和密码分开保存')?.props.disabled, false)
      const confirmed = app.render(name, { code: 'ABCDEF12', saved: true, onSaved() {} })
      assert.equal(confirmed.find(({ props }) => props.type === 'checkbox' || props['aria-label'] === '文件和密码分开保存' || props.accessibilityLabel === '文件和密码分开保存')?.props.disabled, false)
    } finally { app.close() }
  }
})
