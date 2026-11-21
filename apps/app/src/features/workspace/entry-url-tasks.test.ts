
import assert from "node:assert/strict"
import test from "node:test"
import { messageOf, providerNames } from "@vibeface/client/lib/node.ts"


import type { InputOptions, InputTarget, WorkRecord, WorkspaceDomainState } from "@vibeface/client/workspace/types.ts"
import { parseConversation } from "@vibeface/client/workspace/model.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { promptIntent, runtimeLabel } from "@vibeface/client/chat/protocol.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"
import { inboxItems } from "@vibeface/client/workspace/inbox.ts"


import { context, componentHarness, pendingResult, elementText, nestedElements, componentText, parityBot, parityNode, parityWork, parityShared, taskOverviewUi, taskOverviewHarness, settleComponent } from './entry-url-test-kit.test-support.ts'


test('the retained conversation only mounts task history on its exact chat route', () => {
  let pathname = '/b/bot'
  const shared = { ...parityShared(), readWorkspaceState: async () => workspaceModel.parseWorkspaceState({ coverage: { limit: 128, truncated: false } }) }
  const app = componentHarness('./workspace-shell.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: null, directory: null }) },
    '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {}, useRouterState: ({ select }: { select: (state: unknown) => unknown }) => select({ location: { pathname, search: {} } }) },
    './inbox': { useInbox: () => [] },
    '@/features/devices/join': { useOpenJoins: () => [] }, '@vibeface/client/workspace/inbox.ts': { waitingItems: () => [] },
    '@vibeface/client/workspace/home.ts': { backTo: () => undefined }, '@/lib/glossary': { terms: Object.fromEntries(['inbox', 'task', 'myDevices', 'settings'].map((key) => [key, { label: key, icon: key }])) },
    '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle' }, '@vibeface/client/avatar/mood.ts': { moodLabel: { idle: '待命' } },
  }, {}, 'WorkspaceLayout')
  const active = () => app.render('WorkspaceLayout', { children: null, landing: false }).find(({ type }) => type === 'ConversationScreen')?.props.active
  try {
    assert.equal(active(), true)
    for (const route of ['/b/bot/work/work', '/devices', '/tasks', '/b/missing']) { pathname = route; assert.equal(active(), false, route) }
    pathname = '/b/bot'
    assert.equal(active(), true)
  } finally { app.close() }
})


test('closing task detail restores a connected opener or the persistent workspace after history unmounts', () => {
  const focused: string[] = []
  class Target {
    isConnected = true
    readonly name: string
    constructor(name: string) { this.name = name }
    focus(options: { preventScroll: boolean }) { assert.equal(options.preventScroll, true); focused.push(this.name) }
  }
  const body = new Target('body'), main = new Target('main'), link = new Target('history-link')
  const ownerDocument = { activeElement: link, body, getElementById: (id: string) => { assert.equal(id, 'main'); return main } }
  const panel = { ownerDocument, focus: (options: { preventScroll: boolean }) => { assert.equal(options.preventScroll, true); focused.push('detail') } }
  const app = componentHarness('./task-detail.tsx', {}, { HTMLElement: Target }, 'holdFocus')
  try {
    for (const source of ['connected', 'removed', 'body']) {
      ownerDocument.activeElement = source === 'body' ? body : link
      link.isConnected = source === 'connected'
      app.render('holdFocus', panel)
      const close = app.output
      assert.equal(typeof close, 'function')
      if (typeof close === 'function') close()
    }
    assert.deepEqual(focused, ['detail', 'history-link', 'detail', 'main', 'detail', 'main'])
  } finally { app.close() }
})


test('task overview waits for its first task read and simplifies only globally empty loaded records', async () => {
  let reads = 0, creates = 0
  const shared = { ...parityShared(), openWorks: [], data: null, synced: false, readWorkspaceState: async () => { reads++; return { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } } } }
  let inbox: unknown[] = [{ kind: 'setup', key: 'setup', title: '设置运行 Bot 的设备' }]
  shared.overview = workspaceModel.parseWorkspaceOverview({ consistency: 'partial', coverage: { source: 'workspace_overview', truncated: true, omitted_tasks: 1 } })
  const { app, render } = taskOverviewHarness('web', shared, { ...taskOverviewUi, './workspace-shell': { useCreateBot: () => () => { creates++ } } }, () => inbox)
  const named = (name: string) => render().find(({ type }) => typeof type === 'function' && type.name === name)
  const content = () => render().find(({ type, props }) => type === 'div' && Array.isArray(props.children) && props.children.some((child: { type?: unknown }) => child?.type === 'WorkspaceTasks'))!
  const pending = () => render().find(({ type, props }) => type === 'div' && (props.children as { type?: unknown })?.type === 'PendingSection')!
  try {
    assert.equal(content().props.className, 'hidden', 'an initial skeleton replaces the still-mounted task content')
    assert.equal(render().some(({ type }) => type === 'WorkspaceTasks'), true, 'the hidden task component still owns the first read and its draft')
    assert.equal(render().some(({ type }) => type === 'PendingSection'), true)
    assert.equal(pending().props.className, 'hidden', 'ordinary setup feedback appears with the completed first read, without pushing the loading region down')
    assert.equal(named('WorkspaceEmpty'), undefined)
    assert.match(componentText(render()), /正在读取当前连接的任务/)
    assert.match(componentText(render()), /当前连接尚未完成首读/)
    shared.synced = true
    assert.equal(content().props.className, 'hidden', 'overview sync alone does not prove the task conversations are empty')
    const failure = new Error('任务首读失败')
    ;(render().find(({ type }) => type === 'WorkspaceTasks')!.props.onReadError as (cause: unknown) => void)(failure)
    const error = render().find(({ type }) => type === 'InlineError')!.props
    assert.equal(error.error, failure, 'first-read failure is actionable outside the hidden task body')
    assert.equal(pending().props.className, 'contents', 'a failed first read immediately releases known reminders and recovery actions')
    assert.match(componentText(render()), /任务还没读到/)
    ;(error.action as unknown as { onClick: () => void }).onClick()
    await settleComponent()
    assert.equal(reads, 1)
    shared.workspaceState = { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } }
    assert.equal(content().props.className, 'contents')
    assert.equal(pending().props.className, 'contents')
    assert.equal(named('TaskRunSummary'), undefined, 'a setup reminder does not require three all-zero task metrics')
    assert.equal(named('TaskFilters'), undefined)
    assert.equal(named('WorkspaceEmpty'), undefined, 'WorkspaceTasks owns the single empty message and new-task entry')
    assert.equal(render().filter(({ type }) => type === 'WorkspaceTasks').length, 1)
    assert.equal(render().some(({ type }) => type === 'PendingSection'), true, 'the setup reminder stays mounted')
    assert.match(componentText(render()), /部分记录，不代表全部任务/)
    assert.match(componentText(render()), /总览已截断/)
    assert.match(componentText(render()), /本次总览未返回 1 项任务/)
    shared.connectionOnline = false
    assert.match(componentText(render()), /连接已断开，显示最近记录/)
    inbox = [{ kind: 'undelivered', message: { message_id: 'unknown', delivery: 'unknown' } }]
    assert.ok(named('TaskRunSummary'), 'an unknown input keeps the full task surface and PendingSection')
    assert.ok(named('TaskFilters'))
    inbox = []
    shared.bots = []
    const newBot = render().find(({ type, props }) => type === 'Button' && elementText(props.children) === '新 Bot')
    assert.ok(newBot)
    newBot.props.onClick!()
    assert.equal(creates, 1, 'the first Bot remains creatable when no Bot can receive a task')
  } finally { app.close() }
})


test('task overview recovers workspace first-read failure even after the task projection read succeeded', async () => {
  let refreshes = 0, taskReads = 0
  const state: WorkspaceDomainState = { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } }
  const failure = { text: '读取最新内容：nodes 首读失败' }
  const shared = {
    ...parityShared(), openWorks: [] as ReturnType<typeof parityShared>['openWorks'], data: null, synced: false, nodesRead: false, workspaceState: state,
    error: failure as typeof failure | null,
    refresh: async (): Promise<void> => { refreshes++; shared.synced = true },
    readWorkspaceState: async () => { taskReads++; return state },
  }
  const inbox = () => [{ kind: 'setup', key: 'setup', title: '设置运行 Bot 的设备' }]
  const { app, render } = taskOverviewHarness('web', shared, taskOverviewUi, inbox)
  const content = () => render().find(({ type, props }) => type === 'div' && Array.isArray(props.children) && props.children.some((child: { type?: unknown }) => child?.type === 'WorkspaceTasks'))!
  const pending = () => render().find(({ type, props }) => type === 'div' && (props.children as { type?: unknown })?.type === 'PendingSection')!
  try {
    const error = render().find(({ type }) => type === 'InlineError')!.props
    assert.equal(error.error, failure.text)
    assert.equal(render().some(({ props }) => props['aria-busy'] === true), false, 'a failed prerequisite is not an indefinitely busy first read')
    assert.equal(render().some(({ type }) => type === 'Skeleton'), false)
    assert.equal(pending().props.className, 'contents', 'known reminders remain available when the workspace first read failed')
    assert.equal(content().props.className, 'hidden', 'an empty task projection cannot establish global readiness by itself')
    ;(error.action as unknown as { onClick: () => void }).onClick()
    await settleComponent()
    assert.equal(refreshes, 1, 'retry rereads the nodes, bots, overview and conversation prerequisites')
    assert.equal(taskReads, 0, 'retry does not merely reread the already successful task projection')
    assert.equal(shared.error, failure, 'the real workspace retains its global toast object after a successful refresh')
    assert.equal(content().props.className, 'contents')
    assert.equal(render().some(({ type }) => type === 'InlineError'), false, 'successful sync suppresses the stale prerequisite failure')
    shared.synced = false
    shared.openWorks = [{ ...parityWork({ state: 'unverified', outcome: 'unverified' }), conversation_id: 'chat' }]
    assert.equal(content().props.className, 'contents', 'known unknown work remains visible while the first read needs recovery')
    assert.equal(pending().props.className, 'contents')
    assert.equal(render().some(({ type }) => typeof type === 'function' && type.name === 'TaskOverviewCard'), true)
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, failure.text, 'prerequisite recovery remains persistent beside the known work')
  } finally { app.close() }
})


test('task overview ignores a failed refresh completion from a replaced generation', async () => {
  const held = pendingResult()
  const state: WorkspaceDomainState = { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } }
  const shared = { ...parityShared(), openWorks: [], data: null, synced: false, workspaceState: state,
    error: { text: '旧连接首读失败' } as { text: string } | null, refresh: () => held.promise,
  }
  const { app, render } = taskOverviewHarness('web', shared, taskOverviewUi)
  try {
    const error = render().find(({ type }) => type === 'InlineError')!.props
    ;(error.action as unknown as { onClick: () => void }).onClick()
    shared.generation++
    shared.error = null
    render()
    held.reject(new Error('旧连接重读失败'))
    await settleComponent()
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, null)
    assert.equal(render().some(({ type }) => type === 'Skeleton'), true, 'the new connection keeps its own first-read state')
  } finally { app.close() }
})


test('task overview keeps filters for completed business history and empty filter results', () => {
  const state = workspaceModel.parseWorkspaceState({ channels: [], tasks: [{ task_id: 'completed-task', root_task_id: 'completed-task', owner_bot: parityBot.bot_id, title: '已完成的业务任务', state: 'completed', revision: 1, request: '检查项目', expected_result: '交付结果', source_events: [], dependencies: [], reply_only: false, action: 'continue', request_event: 'request', event_hash: 'event', created_at_ms: 1, updated_at_ms: 2 }], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } })
  const other = { ...parityBot, bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' }
  const shared = { ...parityShared(), openWorks: [] as ReturnType<typeof parityShared>['openWorks'], data: null, bots: [parityBot, other], workspaceState: state, nodesRead: false, error: null as { text: string } | null }
  let inbox: unknown[] = []
  const { app, render } = taskOverviewHarness('web', shared, taskOverviewUi, () => inbox)
  const named = (name: string) => render().find(({ type }) => typeof type === 'function' && type.name === name)
  const content = () => render().find(({ type, props }) => type === 'div' && Array.isArray(props.children) && props.children.some((child: { type?: unknown }) => child?.type === 'WorkspaceTasks'))!
  const pending = () => render().find(({ type, props }) => type === 'div' && (props.children as { type?: unknown })?.type === 'PendingSection')!
  try {
    shared.synced = false
    assert.equal(content().props.className, 'hidden', 'saved nonempty tasks do not bypass first-read staging and paint count placeholders')
    inbox = [{ kind: 'setup', key: 'setup', title: '设置运行 Bot 的设备' }]
    assert.equal(pending().props.className, 'hidden', 'a late setup reminder stays in the same first-read reveal as the saved task body')
    shared.nodesRead = true
    assert.equal(content().props.className, 'contents', 'the completed task snapshot remains readable while unrelated conversation reads or write alignment keep synced false')
    assert.equal(pending().props.className, 'contents')
    shared.nodesRead = false
    shared.error = { text: '首读失败' }
    assert.equal(content().props.className, 'contents', 'a failed first read releases known history instead of hiding it indefinitely')
    assert.equal(pending().props.className, 'contents')
    shared.error = null
    shared.connectionOnline = false
    assert.equal(content().props.className, 'contents', 'offline readers keep their known records')
    shared.connectionOnline = true
    shared.synced = true
    assert.equal(content().props.className, 'contents')
    assert.ok(named('TaskRunSummary'), 'completed task history prevents the global-empty shortcut')
    assert.ok(named('TaskFilters'))
    assert.equal(named('WorkspaceEmpty'), undefined, 'completed business tasks count as real content under all tasks')
    const filters = named('TaskFilters')!.props
    ;(filters.onFilter as (value: string) => void)('attention')
    ;(filters.onBotFilter as (value: string) => void)(other.bot_id)
    assert.ok(named('TaskRunSummary'))
    assert.ok(named('TaskFilters'), 'a no-match state retains controls for recovery')
    assert.equal(named('WorkspaceEmpty')?.props.title, '没有符合筛选的任务')
    assert.equal(render().find(({ type }) => type === 'WorkspaceTasks')?.props.botFilter, other.bot_id)
    assert.equal(render().find(({ type }) => type === 'WorkspaceTasks')?.props.filter, 'attention')
    assert.equal(render().find(({ type }) => type === 'WorkspaceTasks')?.props.showEmpty, false, 'the parent owns the only no-match empty message')
    const noMatch = named('WorkspaceEmpty')!
    assert.equal(typeof noMatch.type, 'function')
    const emptyState = (noMatch.type as (props: unknown) => { props: { action: { props: { children: string; onClick: () => void } } } })(noMatch.props)
    assert.equal(emptyState.props.action.props.children, '清除筛选')
    emptyState.props.action.props.onClick()
    assert.equal(named('TaskFilters')?.props.filter, 'all')
    assert.equal(named('TaskFilters')?.props.botFilter, 'all')
    assert.equal(named('WorkspaceEmpty'), undefined, 'clearing filters restores the retained completed task')
    ;(filters.onFilter as (value: string) => void)('attention')
    ;(filters.onBotFilter as (value: string) => void)(other.bot_id)
    shared.workspaceState = { ...state, tasks: [] }
    shared.openWorks = [{ ...parityWork({ state: 'finished', outcome: 'completed' }), conversation_id: 'chat' }]
    assert.ok(named('TaskFilters'), 'history retained outside the current Bot and state filters still prevents global emptiness')
    assert.equal(named('WorkspaceEmpty')?.props.title, '没有符合筛选的任务')
    shared.openWorks = []
    shared.overview = workspaceModel.parseWorkspaceOverview({
      bots: [parityBot, other],
      tasks: [{ item_id: 'remote-work', kind: 'work', work_id: 'remote-work', conversation_id: parityBot.conversation_id, bot: parityBot, target_node: parityNode.device_id, state: 'running', title: '远端任务' }],
    })
    assert.ok(named('TaskFilters'), 'a remote-only task belonging to another Bot still prevents global emptiness')
    assert.equal(named('WorkspaceEmpty')?.props.title, '没有符合筛选的任务')
  } finally { app.close() }
})


test('task conversations distinguish first use from filter no-match and can defer empty feedback to the overview', () => {
  const state: WorkspaceDomainState = { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } }
  const workspace = { ...parityShared(), workspaceState: state, readWorkspaceState: async () => state }
  const app = componentHarness('./workspace-tasks.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace } })
  try {
    assert.match(componentText(app.render('WorkspaceTasks')), /还没有任务对话/)
    const filtered = app.render('WorkspaceTasks', { filter: 'working' })
    assert.match(componentText(filtered), /没有符合筛选的任务对话/)
    assert.doesNotMatch(componentText(filtered), /还没有任务对话/)
    const delegatedEmpty = app.render('WorkspaceTasks', { filter: 'working', showEmpty: false })
    assert.doesNotMatch(componentText(delegatedEmpty), /没有符合筛选|还没有任务对话/)
    assert.match(componentText(delegatedEmpty), /新任务/, 'the task creator stays mounted and usable while its result message is composed by the parent')
    assert.equal(delegatedEmpty.some(({ type }) => typeof type === 'function' && type.name === 'CreateWorkspaceTask'), true)
  } finally { app.close() }
})


test('background task activity uses readable purpose, status and device names with raw IDs only in optional details', () => {
  const trigger = 'message:' + 'f'.repeat(80), source = 'd'.repeat(80)
  const activity = { kind: 'turn', job: { state: 'queued', trigger, source_node: source } }
  const app = componentHarness('./tasks-overview.tsx', { '@vibeface/client/workspace/labels.ts': taskLabels }, {}, 'TurnActivity')
  const lookup = { nodes: [{ ...parityNode, device_id: source, name: '工作电脑' }], directory: null }
  const render = () => app.render('TurnActivity', { activity, lookup })
  const primary = () => {
    const body = render().find(({ type }) => type === 'RecordBody')!.props.children as { type: unknown; props: unknown }[]
    return elementText(body.filter((child) => child?.type !== 'Details'))
  }
  try {
    assert.match(primary(), /处理消息.*排队中.*工作电脑/)
    assert.doesNotMatch(primary(), /queued|message:|ffff|dddd|等待名额|机器繁忙/)
    const elements = render()
    const details = elements.find(({ type }) => type === 'Details')!
    assert.equal(details.props.label, '技术详情')
    assert.equal(details.props.open, undefined)
    assert.match(elementText(details.props.children), new RegExp(trigger))
    assert.match(elementText(details.props.children), new RegExp(source))
    assert.match(String(details.props.className), /overflow-wrap:anywhere/)
    assert.match(String(elements.find(({ type }) => type === 'RecordBody')?.props.className), /min-w-0.*overflow-wrap:anywhere/)
    lookup.nodes = []
    assert.match(primary(), /来源设备 · 未知设备/)
    assert.doesNotMatch(primary(), new RegExp(source))
    for (const [prefix, purpose] of [['task:', '推进任务'], ['schedule:', '定时工作'], ['unknown:', '后台工作']]) {
      activity.job.trigger = prefix + 'f'.repeat(80)
      activity.job.state = 'unknown'
      assert.match(primary(), new RegExp(purpose + '.*状态未知'))
    }
  } finally { app.close() }
})


test('both task overviews preserve known records, show their coverage and only navigate cross-Bot program checks', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    shared.openWorks = Array.from({ length: 257 }, (_, index) => ({ ...parityWork({ work_id: `work-${index}`, prompt: `完整请求 ${index}` }), conversation_id: 'chat' }))
    shared.overview = workspaceModel.parseWorkspaceOverview({
      bots: [parityBot, { bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' }],
      tasks: shared.openWorks.slice(0, 256), truncated: true, consistency: 'partial', checks: [{ check_id: 'other-process', conversation_id: 'other-chat', trigger: 'message:remote' }],
      coverage: { source: 'workspace_overview', truncated: true, omitted_tasks: 1, omitted_questions: 2, remote_owners: [{ owner: 'remote', reason: 'remote_scheduler_not_queried' }] },
    })
    const { app, render, cardName, mastheadName } = taskOverviewHarness(platform, shared, taskOverviewUi)
    try {
      let elements = render()
      const cards = elements.filter(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.equal(cards.length, 257, `${platform} keeps the row outside the bounded overview`)
      assert.equal((cards[256].props.work as WorkRecord).prompt, '完整请求 256')
      assert.match(componentText(elements), /总览已截断/)
      assert.match(componentText(elements), /本次总览未返回 1 项任务/)
      assert.match(componentText(elements), /本次总览未返回 2 项问题/)
      assert.match(componentText(elements), /尚未读取其他设备的后台工作/)
      assert.doesNotMatch(componentText(elements), /未返回 0 项/)
      assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, 'another Bot program check cannot settle against the active conversation')
      assert.equal(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard']).some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === 'other' : props.href === '/b/other')), true)
      shared.nodes = [{ ...parityNode, online: false }]
      elements = render()
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 258)
      const offlineCard = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.ok(offlineCard && typeof offlineCard.type === 'function')
      assert.match(componentText(offlineCard.type(offlineCard.props)), /待确认/)
      shared.nodes = [parityNode]
      shared.connectionOnline = false
      elements = render()
      const staleCard = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.ok(staleCard && typeof staleCard.type === 'function')
      assert.match(componentText(staleCard.type(staleCard.props)), /状态未知/)
      shared.synced = false
      shared.openWorks = []
      shared.overview = workspaceModel.parseWorkspaceOverview({})
      elements = render()
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.working, null)
      assert.doesNotMatch(componentText(elements), /没有未完成的任务/)
      shared.synced = true
      shared.connectionOnline = true
      elements = render()
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.working, 0)
      const empty = elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'WorkspaceEmpty' : 'MobileEmpty'))
      if (platform === 'web') assert.equal(empty, undefined, 'Web waits for WorkspaceTasks first read before claiming no tasks')
      else { assert.ok(empty && typeof empty.type === 'function'); assert.match(componentText(empty.type(empty.props)), /当前记录没有未完成的任务/) }
      assert.equal(elements.some(({ type }) => type === 'PendingSection'), true, `${platform} keeps 待处理 atop the task page`)
      shared.openWorks = ['assigned', 'orphan'].map((work_id) => ({ ...parityWork({ work_id }), conversation_id: parityBot.conversation_id }))
      const assignment = { started_revision: 1, bot_id: parityBot.bot_id, execution_node: parityNode.device_id, bot_owner_epoch: 1, basis: {}, resource_roots: [], automatic: false, state: 'accepted', event_hash: 'assignment-event', created_at_ms: 1 }
      shared.workspaceState = workspaceModel.parseWorkspaceState({
        channels: [], inputs: [], coverage: { limit: 128, truncated: false },
        tasks: [{ task_id: 'task', root_task_id: 'task', owner_bot: parityBot.bot_id, title: '业务任务', request: '核对项目', expected_result: '结论', source_events: [], dependencies: [], reply_only: false, revision: 1, action: 'continue', request_event: 'request', event_hash: 'event', created_at_ms: 1, updated_at_ms: 2, state: 'running' }],
        assignments: [
          { ...assignment, assignment_id: 'assigned', work_id: 'assigned', task_id: 'task' },
          { ...assignment, assignment_id: 'orphan', work_id: 'orphan', task_id: 'not-loaded' },
        ],
      })
      elements = render()
      assert.deepEqual(elements.filter(({ type }) => typeof type === 'function' && type.name === cardName).map(({ props }) => (props.work as WorkRecord).work_id), ['orphan'])
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.working, 2, `${platform} counts the business task and independent work once each`)
    } finally { app.close() }
  }
})


test('both task overviews recover bounded Bot identities only from the current complete catalog', () => {
  for (const platform of ['web', 'native']) {
    const bots = Array.from({ length: 257 }, (_, index) => ({ ...parityBot, bot_id: `bot-${index + 1}`, conversation_id: `chat-${index + 1}`, name: `项目 Bot ${index + 1}` }))
    const shared = { ...parityShared(), conversationId: 'chat-1', bots, openWorks: [], overview: workspaceModel.parseWorkspaceOverview({ bots: bots.slice(0, 256), checks: [{ check_id: 'only-check', conversation_id: 'chat-257', trigger: 'message:one' }], truncated: true }) }
    const { app, render } = taskOverviewHarness(platform, shared, {
      '@/components/kit': { providerIcons: {} },
    })
    const opens = (elements: ReturnType<typeof render>, botId = 'bot-257') => nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard']).some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === botId : props.href === `/b/${botId}`))
    try {
      let elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /项目 Bot 257/)
      assert.equal(opens(elements), true, `${platform} keeps the only navigation for the Bot outside overview.bots`)
      assert.doesNotMatch(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /已删除的 Bot/)
      shared.bots = bots.slice(0, 256)
      elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /未找到所属 Bot/)
      assert.equal(opens(elements), false)
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /程序状态待确认/, 'a missing identity does not remove the check')
      shared.bots = [{ ...bots[256], conversation_id: 'different-chat' }]
      assert.equal(opens(render()), false, 'a Bot from a different conversation cannot supply navigation')
      shared.bots = bots
      shared.synced = false
      shared.generation++
      elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /Bot 身份未加载/)
      assert.doesNotMatch(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /项目 Bot 257|已删除的 Bot/)
      assert.equal(opens(elements), false, 'a retained catalog before this connection first read cannot supply an old network identity')
      shared.bots = [{ ...bots[256], bot_id: 'new-network-bot', name: '新网络 Bot' }]
      shared.synced = true
      elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /新网络 Bot/)
      assert.equal(opens(elements, 'new-network-bot'), true)
      assert.equal(opens(elements), false)
    } finally { app.close() }
  }
})


test('both task overviews retain failure outcomes and causes across unordered snapshots', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const { app, render, cardName, mastheadName } = taskOverviewHarness(platform, shared, taskOverviewUi)
    try {
      for (const observedFailure of [true, false]) {
        const running = { ...parityWork({ summary: '', result_text: '完整输出', failure_reason: '旧快照记录的说明' }), conversation_id: 'chat' }
        const failed = { ...parityWork({ ...running, state: 'finished', outcome: 'failed', failure_reason: '这次执行失败，已保留改动' }), conversation_id: running.conversation_id }
        shared.openWorks = [observedFailure ? running : failed]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...(observedFailure ? failed : running), kind: 'work' }] })
        const elements = render()
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
        assert.ok(card && typeof card.type === 'function')
        const text = componentText(card.type(card.props))
        assert.match(text, /失败/, `${platform} uses the outcome rather than treating finished as success`)
        assert.match(text, /这次执行失败，已保留改动/)
        assert.doesNotMatch(text, /旧快照记录的说明/)
        assert.doesNotMatch(text, /已完成/)
        assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 1)
        assert.equal((card.props.work as WorkRecord).result_text, '完整输出')
      }
      for (const [known, observed] of [['completed', 'unverified'], ['unverified', 'completed'], ['completed', 'failed'], ['failed', 'completed']] as const) {
        shared.openWorks = [{ ...parityWork({ state: known === 'unverified' ? 'unverified' : 'finished', outcome: known, summary: '' }), conversation_id: 'chat' }]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...parityWork({ state: observed === 'unverified' ? 'unverified' : 'finished', outcome: observed }), conversation_id: 'chat', kind: 'work' }] })
        const elements = render()
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
        assert.ok(card && typeof card.type === 'function')
        const text = componentText(card.type(card.props))
        assert.match(text, /待确认|状态未知/)
        assert.doesNotMatch(text, /已完成/)
        assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 1)
      }
    } finally { app.close() }
  }
})


test('both task overviews reconcile raw confirmations and only fold task process checks on the same device', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    shared.conversationId = 'other-chat'
    const entry = { ...parityNode, device_id: 'entry', local: true }
    const inbox = () => inboxItems({ device: { ready: null, joins: [], pendingRecovery: null, rotateHint: false }, workspace: shared })
    const { app, render, cardName, mastheadName } = taskOverviewHarness(platform, shared, taskOverviewUi, inbox)
    const count = (elements: ReturnType<typeof render>) => elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention
    try {
      for (const unreachable of ['offline', 'disconnected']) {
        shared.nodes = [entry, { ...parityNode, online: unreachable !== 'offline', capabilities: { ...parityNode.capabilities, codex_connected: unreachable !== 'disconnected' } }]
        shared.openWorks = [{ ...parityWork({ summary: '' }), conversation_id: 'chat' }]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...shared.openWorks[0], kind: 'work', state: 'finished', outcome: 'failed', failure_reason: '总览记录的失败原因' }] })
        const elements = render()
        assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, `${platform} cannot offer settlement for an already failed work`)
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
        assert.ok(card && typeof card.type === 'function')
        assert.match(componentText(card.type(card.props)), /失败.*总览记录的失败原因/)
        assert.equal(count(elements), 1)
      }
      const unknown = { ...parityWork({ kind: 'task', state: 'unverified', outcome: 'unverified', target_node: 'entry' }), conversation_id: 'chat' }
      const checks = [{ check_id: 'process-work', conversation_id: 'chat', trigger: 'task:work' }]
      for (const variant of ['same', 'outside-bound', 'missing', 'other-conversation', 'other-device', 'running']) {
        const known = variant === 'other-conversation' ? { ...unknown, conversation_id: 'different-chat' } : variant === 'other-device' ? { ...unknown, target_node: 'worker' } : variant === 'running' ? { ...parityWork({ ...unknown, state: 'running', outcome: null }), conversation_id: unknown.conversation_id } : unknown
        shared.nodes = [entry, parityNode]
        shared.openWorks = variant === 'missing' ? [] : [known]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: variant === 'outside-bound' || variant === 'missing' ? [] : [{ ...known, kind: 'task' }], checks })
        const elements = render()
        assert.equal(count(elements), ['same', 'outside-bound', 'missing'].includes(variant) ? 1 : variant === 'running' ? 1 : 2, `${platform} ${variant} preserves exactly the represented checks`)
        if (variant === 'same' || variant === 'outside-bound') {
          assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, `${platform} confirms the real unknown work in 待处理, not again in the list`)
          assert.doesNotMatch(componentText(elements), /程序状态待确认|任务状态待确认/)
        }
        if (variant === 'missing') {
          assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /任务状态待确认/)
          assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, 'a bounded-out work keeps navigation rather than a settlement against the active Bot')
          assert.equal(elements.some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === 'bot' : props.href === '/b/bot')), true)
        }
      }
    } finally { app.close() }
  }
})


test('both work details read terminal shared records outside Main and discard late route responses', async () => {
  for (const platform of ['web', 'native']) {
    const first = pendingResult(), second = pendingResult()
    const calls: { conversation_id: string; work_id: string }[] = []
    const shared = { ...parityShared(), openWorks: [], readWork: (input: { conversation_id: string; work_id: string }) => { calls.push(input); return input.work_id === 'first' ? first.promise : second.promise } }
    shared.data = parseConversation(context)
    let workId = 'first'
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, platform === 'native' ? 'TaskDetail' : '')
    const render = () => app.render('TaskDetail', { botId: 'bot', workId })
    const content = () => { const elements = render(); return componentText(elements) + elements.map(({ props }) => typeof props.text === 'string' ? props.text : '').join('') }
    try {
      render()
      workId = 'second'
      render()
      second.resolve({ work: { ...parityWork({ work_id: 'second', title: 'Task 内已结束工作', state: 'finished', outcome: 'completed', result_text: '完整结果记录' }), target: null, source_events: ['a'.repeat(64)], conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } })
      await settleComponent()
      assert.deepEqual(JSON.parse(JSON.stringify(calls)), [{ conversation_id: 'chat', work_id: 'first' }, { conversation_id: 'chat', work_id: 'second' }])
      assert.match(content(), /完整结果记录/)
      assert.match(content(), /完整执行历史未提供/)
      first.resolve({ work: { ...parityWork({ work_id: 'first', title: '旧路由迟到内容' }), target: null, source_events: ['b'.repeat(64)], conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } })
      await settleComponent()
      assert.match(content(), /完整结果记录/)
      assert.doesNotMatch(content(), /旧路由迟到内容/)
      assert.equal(shared.data.works.length, 0, 'detail reads never refill Main')
      assert.equal(shared.openWorks.length, 0, 'terminal history does not depend on the open-work list')
    } finally { app.close() }
  }
})


test('terminal work followups retain their Task identity or explicitly create a task from the original request', async () => {
  for (const platform of ['web', 'native']) for (const kind of ['task', 'channel_task', 'raw', 'unverified']) {
    const target: InputTarget | null = kind === 'task' ? { kind: 'task', task_id: 'stable-task' } : kind === 'channel_task' ? { kind: 'channel_task', task_id: 'stable-task', channel_id: 'channel', bot_id: 'bot' } : null
    const work = kind === 'unverified' ? parityWork({ state: 'unverified' }) : parityWork({ state: 'finished', outcome: 'failed', prompt: '原始完整交代', result_text: '上次已经产生的输出' })
    const sends: { text: string; options: InputOptions }[] = [], reads: unknown[] = []
    const shared = { ...parityShared(), data: parseConversation(context), openWorks: [],
      readWork: async () => ({ work: { ...work, target, source_events: ['a'.repeat(64)], conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } }),
      readWorkspaceState: async (filter: unknown) => { reads.push(filter); return { tasks: [{ task_id: 'stable-task', revision: 3, action: 'continue' }] } },
      sendMessage: async (text: string, _files: unknown[], _sent: unknown, options: InputOptions) => { sends.push({ text, options }); return true },
    }
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, action: () => Promise<void>) => action() }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, platform === 'native' ? 'TaskDetail' : '')
    const render = () => app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    try {
      render(); await settleComponent()
      const elements = render()
      const composer = elements.find(({ type }) => type === (platform === 'web' ? 'PromptInput' : 'Composer'))
      assert.ok(composer)
      if (kind === 'unverified') {
        assert.equal(platform === 'web' ? composer.props.disabled : Boolean(composer.props.blocked), true, 'an unfinished Work that cannot take a supplement blocks its input')
        assert.equal(platform === 'web' ? composer.props.reason : composer.props.blocked, '这个工作当前不能接收补充。')
        continue
      }
      assert.ok(!(platform === 'web' ? composer.props.disabled : composer.props.blocked), 'a terminal Work still accepts a followup')
      assert.equal(composer.props.label, '发送为新任务')
      assert.equal(composer.props.sendLabel, '发送为新任务')
      const follow = elements.find(({ type, props }) => type === (platform === 'web' ? 'Button' : 'ActionButton') && componentText(props.children) === (target ? '继续所属任务' : '新任务重做'))
      assert.ok(follow)
      if (platform === 'web') { assert.ok(follow.props.onClick); follow.props.onClick(); await settleComponent() }
      else { assert.ok(follow.props.action); await follow.props.action() }
      assert.equal(sends.length, 1)
      assert.deepEqual(JSON.parse(JSON.stringify(sends[0].options.target)), target ?? { kind: 'work', work_id: 'work' })
      assert.equal(sends[0].options.intent, target ? 'continue' : 'new_task')
      assert.equal(sends[0].options.expected_revision, target ? 3 : undefined)
      assert.match(sends[0].text, /原始完整交代/)
      assert.match(sends[0].text, /上次已经产生的输出/)
      assert.deepEqual(JSON.parse(JSON.stringify(reads)), target ? [{ task_id: 'stable-task' }] : [])
      const prompt = render().find(({ type }) => type === (platform === 'web' ? 'PromptInput' : 'Composer'))?.props
      prompt?.onChange?.('接着改')
      const submit = render().find(({ type }) => type === (platform === 'web' ? 'PromptInput' : 'Composer'))?.props[platform === 'web' ? 'onSubmit' : 'onSend']
      assert.ok(typeof submit === 'function'); submit(); await settleComponent()
      assert.equal(sends.length, 2)
      assert.equal(sends[1].text, '接着改')
      assert.deepEqual(JSON.parse(JSON.stringify(sends[1].options.target)), { kind: 'work', work_id: 'work' })
      assert.equal(sends[1].options.intent, 'new_task', 'a terminal Work followup is sent as a new task')
    } finally { app.close() }
  }
})


test('both task details bind approval to current unfinished work and keep stops above approval', async () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const approval = { target: { kind: 'work', work_id: 'work' }, question_id: 'approval', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '继续吗', choices: [], author: 'worker', created_at_ms: 2, answer: null, closed: null, about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' } }
    shared.questions = workspaceModel.parseQuestions({ questions: [approval] })
    shared.data = parseConversation({ ...context, works: [parityWork({ work_id: 'old', state: 'finished', outcome: 'completed' }), parityWork()], questions: [approval] })
    let workId = 'old'
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }), useLocalSearchParams: () => ({ botId: 'bot', workId }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, platform === 'native' ? 'TaskDetail' : '')
    const render = async () => { app.render('TaskDetail', { botId: 'bot', workId }); await settleComponent(); return app.render('TaskDetail', { botId: 'bot', workId }) }
    try {
      assert.equal((await render()).some(({ type }) => type === 'QuestionCard'), false, `${platform} old work cannot inherit the later approval`)
      workId = 'work'
      let elements = await render()
      assert.equal(elements.some(({ type }) => type === 'QuestionCard'), true)
      assert.match(componentText(elements), /需要你同意/)
      assert.equal(elements.some(({ props }) => props.label === '最近步骤摘要 · 3'), true)
      shared.answered = { approval: '继续' }
      assert.equal((await render()).some(({ type }) => type === 'QuestionCard'), false)
      shared.answered = {}
      shared.data = parseConversation({ ...context, works: [parityWork()], deliveries: [{ work_id: 'work', target_node: 'worker', state: 'stop_waiting' }], questions: [approval] })
      elements = await render()
      assert.match(componentText(elements), /停止中/)
      assert.doesNotMatch(componentText(elements), /需要你同意/)
      shared.nodes = [{ ...parityNode, capabilities: { ...parityNode.capabilities, codex_connected: false } }]
      assert.match(componentText(await render()), /待确认/)
      shared.nodes = [parityNode]
      shared.data = parseConversation({ ...context, works: [parityWork({ kind: 'task', coordination_job: { conversation_id: 'chat', scope: { kind: 'task', work_id: 'work' }, trigger: 'task:work', kind: 'workspace_task', state: 'queued', sequence: 1, lane: 'task', source_node: 'worker', owner: 'worker', message_id: null, reply_message_id: 'reply', hold_reason: 'machine_slot' } })] })
      elements = await render()
      assert.equal(elements.some(({ props }) => props.label === '协调回合'), true)
      assert.match(componentText(elements), /等待设备运行名额/)
      assert.equal(shared.data.works[0].state, 'running', 'queue observations leave DAG work state intact')
    } finally { app.close() }
  }
})


test('context shows scheduler observations without promising native resume or repeating external sessions', () => {
  const shared = parityShared()
  const data = parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:queued', state: 'queued' }], unanswered: [] } })
  const dependencies = { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/chat/protocol.ts': { promptIntent, runtimeLabel }, '@vibeface/client/workspace/bot-settings.ts': { scheduleFrequency: () => '一次', formatWhen: () => '刚才' }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } }
  const app = componentHarness('./context-panel.tsx', dependencies)
  try {
    const elements = app.render('ContextPanel', { bot: parityBot, data, nodes: shared.nodes, connected: true, onBack() {} })
    assert.equal(elements.filter(({ type }) => type === 'CollaborationPanel').length, 1)
    const continuity = elements.find(({ type }) => typeof type === 'function' && type.name === 'AgentContinuity')
    assert.ok(continuity && typeof continuity.type === 'function')
    let tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /排队中/)
    assert.doesNotMatch(componentText(tree), /正在运行|设备默认模型 · 设备默认模型/)
    data.scheduler!.jobs = [{ scope: { kind: 'main' as const }, trigger: 'message:stop', state: 'cancelling' }]
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /停止中/)
    data.scheduler!.jobs = [{ scope: { kind: 'main' as const }, trigger: 'message:publish', state: 'publishing' }]
    assert.match(componentText(continuity.type(continuity.props)), /回复同步中/)
    data.scheduler!.provider = 'claude'
    data.scheduler!.model = 'new-device-default'
    data.scheduler!.jobs = [{ scope: { kind: 'main' as const }, trigger: 'message:read-back', state: 'reconciling', runtime: workspaceModel.parseTurnRuntime({ provider: 'codex', model: 'frozen-model', reasoning_effort: 'high', generation: 'config', session_generation: 1, delivery_id: 'delivery', stage: 'read_back', execution: 'unknown', attempted_at_ms: 1, switched_from: null, switch_reason: null }) }]
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /正在查询上次回复/)
    assert.match(componentText(tree), /Codex · 提交模型 frozen-model/)
    assert.doesNotMatch(componentText(tree), /new-device-default|Claude Code/)
    data.scheduler = parseConversation({ ...context, scheduler: { scope_jobs: [], provider: 'codex', model: 'current-model', unanswered: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:reconcile', state: 'reconciling', runtime: { provider: 'claude', model: 'frozen-model', reasoning_effort: 'high', generation: 'generation', session_generation: 1, delivery_id: 'delivery', stage: 'read_back', execution: 'unknown', attempted_at_ms: 1, switched_from: null, switch_reason: null } }] } }).scheduler
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /正在查询上次回复/)
    assert.match(componentText(tree), /Claude Code.*frozen-model/)
    assert.doesNotMatch(componentText(tree), /current-model/)
    const runtime = data.scheduler?.jobs[0]?.runtime
    assert.ok(runtime)
    for (const state of ['publishing', 'pending_delivery']) {
      data.scheduler = parseConversation({ ...context, scheduler: { scope_jobs: [], unanswered: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:history', state: 'completed', runtime: { ...runtime, model: 'newer-history', attempted_at_ms: 2 } }, { scope: { kind: 'main' as const }, trigger: 'message:active', state, runtime }] } }).scheduler
      tree = continuity.type(continuity.props)
      assert.match(componentText(tree), /Claude Code.*frozen-model/)
      assert.doesNotMatch(componentText(tree), /newer-history/)
    }
    data.scheduler = null
    assert.match(componentText(continuity.type(continuity.props)), /后台工作状态未提供/)
    assert.equal(elements.some(({ type, props }) => typeof type === 'function' && type.name === 'ContinuityStat' && props.label === '后台工作' && props.value === null), true)
  } finally { app.close() }

  const collaboration = componentHarness('./collaboration-panel.tsx', { ...dependencies,
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/lib/node.ts': { providerNames }, '@/features/devices/ai-settings': { providerIcons: { codex: 'CodexIcon' } },
  })
  assert.throws(() => parseConversation({ ...context, scheduler: { scope_jobs: [], unanswered: [],
    jobs: [{ scope: { kind: 'main' }, trigger: 'message:unsupported', state: 'future' }] } }))
  const records = parseConversation({ ...context,
    scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:q', state: 'queued' }, { scope: { kind: 'main' as const }, trigger: 'message:p', state: 'publishing' }, { scope: { kind: 'main' as const }, trigger: 'message:u', state: 'unknown' }, { scope: { kind: 'main' as const }, trigger: 'message:d', state: 'completed' }], unanswered: [] },
    sessions: [{ node: 'worker', agent: 'codex', session_id: 'external', cwd: '/shared', summary: '外部摘要', muted: true, created_at_ms: 1 }],
    sent: [{ message_id: 'sent', to: 'chat', text: '协作请求', created_at_ms: 2 }],
  })
  try {
    const elements = collaboration.render('CollaborationPanel', { data: records, botId: 'bot' })
    assert.equal(elements.find(({ type, props }) => typeof type === 'function' && type.name === 'Section' && props.title === '未结束的后台工作')?.props.count, 3)
    assert.equal(elements.some(({ type, props }) => type === 'Details' && props.label === '后台工作历史 · 1'), true)
    const publishing = elements.find(({ type, props }) => typeof type === 'function' && type.name === 'JobRow' && (props.job as { state: string }).state === 'publishing')
    assert.ok(publishing && typeof publishing.type === 'function')
    assert.match(componentText(publishing.type(publishing.props)), /回复同步中/)
    const sessions = elements.filter(({ type }) => typeof type === 'function' && type.name === 'SessionRows')
    assert.equal(sessions.length, 1)
    assert.ok(typeof sessions[0].type === 'function')
    shared.nodes = [{ ...parityNode, revoked: true }]
    const sessionTree = sessions[0].type({ ...sessions[0].props, nodes: shared.nodes })
    assert.match(componentText(sessionTree), /已静音/)
    assert.match(componentText(sessionTree), /设备已撤权/)
    const messages = elements.find(({ type }) => typeof type === 'function' && type.name === 'CollaborationMessages')
    assert.ok(messages && typeof messages.type === 'function')
    messages.type(messages.props)
    const sent = elements.find(({ type }) => typeof type === 'function' && type.name === 'SentMessage')
    assert.ok(sent && typeof sent.type === 'function')
    const sentTree = sent.type(sent.props)
    assert.match(componentText(sentTree), /对方是否收到或回复尚不清楚/)
    assert.equal(elements.some(({ type, props }) => type === 'Link' && (props.params as { botId?: string })?.botId === 'bot'), true)
  } finally { collaboration.close() }
})


test('context on both platforms separates compilation, sending, native cache and committed memory', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const projection = {
      attempt_id: 'attempt', provider: 'codex', stage: 'compiled', session_generation: 4,
      scope: { kind: 'main' }, sources: [], sources_omitted: 0, inputs: [], inputs_omitted: 0, stable_fingerprint: 'a'.repeat(64), dynamic_fingerprint: 'b'.repeat(64), stable_bytes: 100, dynamic_bytes: 200,
      invalidation_reason: 'stable_prompt_or_provider_changed', session_status: { resumable: true, pending_generation: true },
      coverage: { messages_omitted: 1, works_omitted: 0, dispatches_omitted: 0, questions_omitted: 0, attachments_omitted: 0, memory_omitted: 3, checkpoint_omitted: 1, handoff_omitted: 0, pending_checks_omitted: 2, bot_indexes_omitted: { skills: 4 }, history_retrieval: 'context' },
      native_token: 'private-native-token', rendered: 'private-prompt',
    }
    shared.data = parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [], unanswered: [], context: projection }, bot_settings: {
      skills: [], schedules: [], memory_staged_count: 2, memory_sync_conflicts: { items: [], has_more: false },
      memory_conflicts: [{ history_id: 'conflict', call_ref: 'remember', memory_id: 'memory', expected_epoch: 1, actual_epoch: 1, expected_revision: 'old', actual_revision: 'new', reason: 'revision_changed' }],
      memory: [{ memory_id: 'memory', text: '已保存的事实', metadata: { epoch: 1, revision: 'a'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: true, source_event_hash: 'b'.repeat(64) } }],
    } })
    const dependencies = {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/bot-settings.ts': { scheduleFrequency: () => '一次', formatWhen: () => '刚才' },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }, '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, SettingsGroup: 'SettingsGroup', SettingsRow: 'SettingsRow', Details: 'Details' },
    }
    const app = componentHarness(platform === 'web' ? './context-panel.tsx' : '../../../../../apps/mobile/app/b/[botId]/context.tsx', dependencies)
    const render = () => nestedElements(app.render(platform === 'web' ? 'ContextPanel' : 'default', { bot: parityBot, data: shared.data, nodes: shared.nodes, connected: shared.connectionOnline, memory: shared, onBack() {} }), ['AgentContinuity', 'ContinuityLayer', 'ContextDetails', 'MemorySummary', 'ContinuityStat', 'ContextRecords', 'Fold'])
    try {
      let elements = render()
      const text = componentText(elements)
      assert.match(text, /已编译/)
      assert.match(text, /会话代次 4/)
      assert.match(text, /下一轮生效/)
      assert.match(JSON.stringify(elements), /下轮重建/)
      assert(!JSON.stringify(elements).includes('可尝试续接'), 'a pending configuration cannot promise resuming the old identity')
      assert.match(text, /已保存的事实/)
      assert.match(text, /由你维护，受保护/)
      assert.match(text, /记忆已被更新，未覆盖新值/)
      assert(!JSON.stringify(elements).includes('private-native-token'))
      assert(!JSON.stringify(elements).includes('private-prompt'))
      shared.data!.scheduler!.context!.stage = 'started'
      assert.match(componentText(render()), /已发送/)
      shared.data!.scheduler!.context!.session_status = { resumable: true, pending_generation: false }
      assert.match(JSON.stringify(render()), /可尝试续接/)
      shared.connectionOnline = false
      elements = render()
      assert.match(JSON.stringify(elements), /当前状态未知/)
      assert(!JSON.stringify(elements).includes('可尝试续接'), 'a disconnected client does not treat cached reachability as a current observation')
      shared.connectionOnline = true
      shared.data!.scheduler!.context!.session_status = null
      elements = render()
      assert.match(JSON.stringify(elements), /续接状态未知/)
      shared.data!.scheduler!.remote = { owner: 'owner', reachable: false, jobs: [], scope_jobs: [], memory_conflicts: [] }
      assert.match(JSON.stringify(render()), /当前状态未知/)
    } finally { app.close() }
  }
})


test("task filters leave live confirmations and questions to 待处理 and keep the empty state within the selected Bot", () => {
  const bots = [{ bot_id: "a", conversation_id: "chat-a", name: "Bot A" }, { bot_id: "b", conversation_id: "chat-b", name: "Bot B" }]
  const work = (conversation_id: string) => ({ kind: "work", work_id: conversation_id, check_id: conversation_id, conversation_id, target_node: "offline", title: conversation_id, detail: "", reason: "offline" })
  let inbox = [
    { key: "device", kind: "confirm", check: { kind: "device", works: [work("chat-a"), work("chat-b")] } },
    { key: "question-b", kind: "question", done: false, question: { conversation_id: "chat-b", bot: bots[1], about: {} } },
    { key: "process", kind: "confirm", check: { kind: "process", check_id: "process", detail: "still running" } },
  ]
  const workspace = { bots, selectedBot: bots[1], openWorks: [], nodes: [], questions: [], liveProgress: [], connectionOnline: true, synced: true, answered: {} }
  const app = componentHarness("./tasks-overview.tsx", {
    "@vibeface/client/workspace/context.tsx": { useWorkspace: () => workspace },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
    "@vibeface/client/workspace/model.ts": workspaceModel,
    "@vibeface/client/workspace/questions.ts": workspaceQuestions,
    "@/lib/utils": { cn: (...values: unknown[]) => values.filter(Boolean).join(" ") },
    "./inbox": { useInbox: () => inbox },
    "./workspace-shell": { useCreateBot: () => () => {} },
  })
  const named = (elements: ReturnType<typeof app.render>, name: string) => elements.find(({ type }) => typeof type === "function" && type.name === name)
  try {
    let elements = app.render("TasksOverview")
    assert.equal(elements.some(({ type }) => type === "PendingSection"), true)
    assert.equal(named(elements, "TaskRunSummary")?.props.attention, 2, "live checks and questions act in 待处理 and still count")
    const selectBot = named(elements, "TaskFilters")?.props.onBotFilter
    assert.ok(typeof selectBot === "function")
    selectBot("a")
    elements = app.render("TasksOverview")
    assert.equal(named(elements, "TaskRunSummary")?.props.attention, 1)
    assert.equal(nestedElements(elements, ["AttentionActivity"]).some(({ type }) => type === "ConfirmCard" || type === "QuestionCard"), false)
    assert.equal(named(elements, "WorkspaceEmpty"), undefined, "the task list does not claim emptiness under waiting items")
    inbox = []
    elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceEmpty"), undefined, "without task-conversation first read, removing the last pending item returns to loading")
  } finally { app.close() }
})
