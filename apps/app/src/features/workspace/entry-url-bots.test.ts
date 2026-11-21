import type { GenerationAction } from '@vibeface/client/lib/use-generation-action.ts'
import assert from "node:assert/strict"
import test from "node:test"
import { entryAddress, entryKey } from "@vibeface/client/lib/node.ts"



import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { botReadiness, pendingChecks } from "@vibeface/client/workspace/model.ts"
import { folderLabels, folderSetup, placeChanges, projectChoices, projectKey, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { awaitsAnswer, foldAnswers, projectName, questionOpen, sharedProject } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { followUp, followUpCopy, promptSummary } from "@vibeface/client/workspace/labels.ts"
import { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { setUpOnManager } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread, taskMessage } from "@vibeface/client/chat/timeline.ts"
import { primaryLane } from "@vibeface/client/identity/connection-lane.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"


import { context, historyActions, componentHarness, pendingResult, elementText, parityWork, parityShared, settleComponent } from './entry-url-test-kit.test-support.ts'


test("any entrance stops an unfinished task once, after a confirming second tap, even on another device", async () => {
  const stopped: string[] = []
  const data = { local_node: 'worker', works: [] as ReturnType<typeof parityWork>[], questions: [] }
  const workspace = { ...parityShared(), generation: 1, nodes: [], liveProgress: [], writable: true, pendingMessages: [], canSaveMessages: true, questions: [], data, interruptWork: async (work: string) => { stopped.push(work) }, async readWork(input: { conversation_id: string; work_id: string }) {
    const work = data.works.find((item) => item.work_id === input.work_id)
    if (!work) throw new Error('共享历史中找不到这个工作')
    return { work: { ...work, target: work.target ?? null, source_events: ['a'.repeat(64)], conversation_id: input.conversation_id }, coverage: { source: 'shared_history', complete: false } }
  } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/arc/product-kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { ...taskLabels, nodeName: () => '设备', promptSummary, taskActive: () => true, taskLabel: () => '', taskStale: () => false, staleTaskLabel: '', progressLabel: () => '', shownState: (work: { state: string }) => work.state, taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': workspaceQuestions }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  const render = async () => { workspace.data = { ...data }; workspace.openWorks = data.works.map((item) => ({ ...item, conversation_id: 'chat' })); app.render('TaskDetail', { botId: 'bot', workId: 'work' }); await settleComponent(); return app.render('TaskDetail', { botId: 'bot', workId: 'work' }) }
  const stop = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'danger')
  const ask = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'secondary' && [props.children].flat().some((child) => typeof child === 'string' && child.includes('停止')))
  try {
    const work = parityWork({ target_node: 'worker', cwd: '/p', title: '工作', prompt: '工作', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null })
    data.works = [{ ...work, state: 'unverified' }]
    assert.equal(stop(await render()), undefined)
    data.works = [{ ...work, target_node: 'other', stop: 'stop_waiting' }]
    assert.equal(stop(await render()), undefined, "a stop already on its way is not sent twice")
    data.works = [{ ...work, target_node: 'other' }]
    const first = await render()
    assert.equal(stop(first), undefined, 'one tap never stops a task')
    ;(ask(first)?.props.onClick as () => void)()
    ;(stop(await render())?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work'])
    const words = (elements: { props: Record<string, unknown> }[]) => elements.flatMap(({ props }) => [props.children].flat()).filter((child) => typeof child === 'string' || typeof child === 'number').join('')
    const turn = { thread_id: 'th', turn_id: 'tu' }
    data.works = [{ ...work, ...turn }, { ...work, ...turn, work_id: 'steered' }]
    const elements = await render()
    assert.match(words(elements), /会一起停止/, 'a shared turn lists what stops with it')
    assert.match(words(elements), /一起停止 2 个任务/)
    ;(ask(elements)?.props.onClick as () => void)()
    ;(stop(await render())?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work', 'work'], 'the count is named before the confirming tap stops them all')
  } finally { app.close() }
})


test("task detail delegates narrow-screen modal behavior to Arc and remains an inline inspector on wide screens", () => {
  let wide = false, closed = 0
  const workspace = { ...parityShared(), generation: 1, nodes: [], liveProgress: [], writable: true, pendingMessages: [], canSaveMessages: true, questions: [], data: { local_node: 'worker', questions: [], works: [] } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => { closed++ }, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/arc/product-kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { ...taskLabels, nodeName: () => '设备', shownState: () => '', taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': workspaceQuestions }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: wide }) } })
  const render = () => app.render('TaskDetail', { botId: 'bot', workId: 'work' })
  try {
    const narrow = render()
    const drawer = narrow.find(({ type }) => type === 'Drawer')
    assert.equal(drawer?.props.open, true)
    assert.equal(narrow.find(({ type }) => type === 'DrawerContent')?.props.title, '任务')
    ;(drawer?.props.onOpenChange as (open: boolean) => void)(false)
    assert.equal(closed, 1, 'Escape, outside click and Arc close return through the route callback')
    wide = true
    const beside = render()
    assert.equal(beside.some(({ type }) => type === 'Drawer'), false)
    assert.equal(beside.find(({ type }) => type === 'aside')?.props['aria-label'], '任务详情')
  } finally { app.close() }
})


test("bot projects use online manager capabilities and preserve device names", () => {
  const node = { device_id: 'worker', name: '工作电脑', local: false, online: true, revoked: false, capabilities: { manager: 'codex', roots: ['/path/to/a/long/path/to/project'] } }
  const app = componentHarness('./bot-form.tsx', { '@tanstack/react-router': { useNavigate: () => () => {} }, '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ generation: 1, nodes: [node, { ...node, device_id: 'no-agent', name: '无 Agent', capabilities: { manager: null, roots: ['/unavailable'] } }], pendingBot: null, writable: true }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { nodeName }, '@vibeface/client/workspace/questions.ts': { projectName } })
  try {
    const projects = app.render('BotForm', { open: true, onOpenChange() {}, restoreFocus() {} }).find(({ type }) => type === 'Select')?.props.options as { value: string; label: string }[]
    const named = projects.filter(option => option.value !== 'arc:')
    assert.equal(named.length, 1)
    assert.equal(named[0].label, '工作电脑 · project', 'the folder name stays readable in a narrow select')
  } finally { app.close() }
})


test("unsent bot fields survive drawer dismissal and project setup until cancel or confirmed creation", async () => {
  let open = true, fail = true
  const ready = { principal: { can_manage: true, source: 'membership' }, entry: { device_id: 'worker' } }
  const node = { device_id: 'worker', name: '工作电脑', local: false, online: true, revoked: false, capabilities: { manager: 'codex', roots: ['/project'] } }
  const payloads: unknown[] = []
  const workspace = { generation: 1, nodes: [] as typeof node[], pendingBot: null, writable: true, isCurrent: () => true,
    createBot: async (payload: unknown) => { payloads.push(payload); if (fail) throw new Error('暂时没建成'); return { bot_id: 'created' } },
  }
  const app = componentHarness('./bot-form.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => {} },
    '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready }) },
    '@vibeface/client/workspace/labels.ts': { nodeName },
    '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message },
  })
  const render = () => app.render('BotForm', { open, onOpenChange: (next: boolean) => { open = next }, restoreFocus() {} })
  const field = (name: string) => render().find(({ props }) => props.name === name)!.props
  const submit = () => render().find(({ type }) => type === 'form')!.props.onSubmit!({ preventDefault() {} })
  const reopen = () => { render(); open = true; render(); return render() }
  const expectDraft = () => {
    assert.equal(field('bot-name').value, '项目 Bot')
    assert.equal(field('bot-instructions').value, '维护这个项目，保留未发送说明。')
  }
  try {
    render()
    field('bot-name').onChange!({ target: { value: '项目 Bot' } })
    field('bot-instructions').onChange!({ target: { value: '维护这个项目，保留未发送说明。' } })
    const setupLink = render().find(({ type }) => type === 'Link')!
    assert.equal(setupLink.props.to, '/devices/$deviceId')
    setupLink.props.onClick!()
    assert.equal(open, false)
    workspace.nodes = [node]
    reopen(); expectDraft()
    const project = render().find(({ type }) => type === 'Select')!.props
    ;(project.onValueChange as (value: string) => void)('arc:' + projectKey({ target_node: 'worker', cwd: '/project' }))
    ;(render().find(({ type }) => type === 'Drawer')!.props.onOpenChange as (next: boolean) => void)(false)
    reopen(); expectDraft()
    assert.equal(render().find(({ type }) => type === 'Select')!.props.value, 'arc:' + projectKey({ target_node: 'worker', cwd: '/project' }))
    submit(); await settleComponent(); expectDraft()
    assert.equal(open, true, 'a failed creation leaves the draft available')
    fail = false
    submit(); await settleComponent()
    assert.equal(open, false)
    assert.deepEqual(JSON.parse(JSON.stringify(payloads)), Array(2).fill({ name: '项目 Bot', instructions: '维护这个项目，保留未发送说明。', project: { target_node: 'worker', cwd: '/project' } }))
    reopen()
    assert.equal(field('bot-name').value, '')
    assert.equal(field('bot-instructions').value, '')
    assert.equal(render().find(({ type }) => type === 'Select')!.props.value, 'arc:')
    field('bot-name').onChange!({ target: { value: '明确放弃的草稿' } })
    render().find(({ type, props }) => type === 'Button' && props.children === '取消')!.props.onClick!()
    reopen()
    assert.equal(field('bot-name').value, '', 'explicit cancel discards the unsent draft')
  } finally { app.close() }
})


test("bot name validation focuses its field and clears only when the name changes", async () => {
  let focused = 0, creates = 0
  const workspace = { generation: 1, nodes: [], pendingBot: null, writable: true, isCurrent: () => true, createBot: async () => { creates++; throw new Error('创建暂时失败') } }
  const app = componentHarness('./bot-form.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => {} },
    '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message },
  })
  const render = () => app.render('BotForm', { open: true, onOpenChange() {}, restoreFocus() {} })
  const field = (name: string) => render().find(({ props }) => props.name === name)!.props
  const submit = () => render().find(({ type }) => type === 'form')!.props.onSubmit!({ preventDefault() {} })
  try {
    render()
    const name = field('bot-name')
    ;(name.ref as { current: unknown }).current = { focus: () => { focused++ } }
    submit()
    assert.equal(focused, 1)
    assert.equal(creates, 0, 'an empty name does not submit a create operation')
    assert.equal(field('bot-name').error, '还没填名称', 'Arc Input associates this error with the name field')
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, '', 'the field error is not duplicated at the bottom')
    field('bot-instructions').onChange!({ target: { value: '可选说明' } })
    assert.equal(field('bot-name').error, '还没填名称', 'editing an unrelated optional field keeps the name error')
    field('bot-name').onChange!({ target: { value: '项目 Bot' } })
    assert.equal(field('bot-name').error, '')
    submit(); await settleComponent()
    assert.equal(creates, 1)
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, '创建暂时失败')
    field('bot-name').onChange!({ target: { value: '另一个 Bot' } })
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, '创建暂时失败', 'editing a name does not discard an operation failure')
  } finally { app.close() }
})


test("bot unknown submissions retain their read-only saved payload for reconciliation", async () => {
  let open = true
  const pending = { bot_id: 'pending-bot', name: '已提交名称', instructions: '已提交说明', project: null, submission: { state: 'unknown' } }
  const payloads: unknown[] = [], closed: boolean[] = []
  const app = componentHarness('./bot-form.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => {} },
    '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ generation: 1, nodes: [], pendingBot: pending, writable: true, isCurrent: () => true, createBot: async (payload: unknown) => { payloads.push(payload); throw new Error('结果仍未知') } }) },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message },
  })
  const render = () => app.render('BotForm', { open, onOpenChange: (next: boolean) => { open = next; closed.push(next) }, restoreFocus() {} })
  try {
    render(); const elements = render()
    const name = elements.find(({ props }) => props.name === 'bot-name')!
    assert.equal(name.props.readOnly, true)
    assert.equal(name.props.value, pending.name)
    assert.equal(name.props.error, '')
    assert.equal(elements.find(({ props }) => props.name === 'bot-instructions')?.props.readOnly, true)
    assert.equal(elements.find(({ props }) => props.id === 'bot-project')?.props.readOnly, true)
    assert.match(elementText(elements.find(({ type }) => type === 'DrawerContent')?.props.description), /上次没确认是否建好/)
    elements.find(({ type }) => type === 'form')!.props.onSubmit!({ preventDefault() {} })
    await settleComponent()
    assert.deepEqual(JSON.parse(JSON.stringify(payloads)), [{ name: pending.name, instructions: pending.instructions, project: pending.project }])
    assert.deepEqual(closed, [], 'an unknown outcome leaves the reconciliation form open')
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, '结果仍未知')
    render().find(({ type, props }) => type === 'Button' && props.children === '取消')!.props.onClick!()
    render(); open = true; render()
    const reopened = render()
    assert.equal(reopened.find(({ props }) => props.name === 'bot-name')?.props.value, pending.name)
    assert.equal(reopened.find(({ props }) => props.name === 'bot-name')?.props.readOnly, true, 'cancel cannot turn an unknown submission into an editable draft')
    reopened.find(({ type }) => type === 'form')!.props.onSubmit!({ preventDefault() {} })
    await settleComponent()
    assert.deepEqual(JSON.parse(JSON.stringify(payloads[1])), { name: pending.name, instructions: pending.instructions, project: pending.project })
  } finally { app.close() }
})


test("bot reconciliation clears confirmed drafts and preserves proven rejected drafts even while closed", async (t) => {
  for (const hidden of [false, true]) for (const confirmed of [false, true]) await t.test(`${hidden ? 'closed' : 'open'} / ${confirmed ? 'confirmed' : 'rejected with no matching Bot'}`, async () => {
    let open = true
    const pending = { bot_id: 'submitted-bot', name: '待确认 Bot', instructions: '只创建一次', project: { target_node: 'worker', cwd: '/project' }, submission: { state: confirmed ? 'unconfirmed' : 'rejected' } }
    const node = { device_id: 'worker', name: '工作电脑', local: false, online: true, revoked: false, capabilities: { manager: 'codex', roots: ['/project'] } }
    const payloads: unknown[] = []
    const workspace = { generation: 1, nodes: [node], bots: [] as { bot_id: string; name: string }[], pendingBot: null as typeof pending | null, writable: true, isCurrent: () => true,
      createBot: async (payload: unknown) => { payloads.push(payload); throw new Error('停止在提交边界') },
    }
    const app = componentHarness('./bot-form.tsx', {
      '@tanstack/react-router': { useNavigate: () => () => {} },
      '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/workspace/labels.ts': { nodeName },
      '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message },
    })
    const render = () => app.render('BotForm', { open, onOpenChange: (next: boolean) => { open = next }, restoreFocus() {} })
    const field = (name: string) => render().find(({ props }) => props.name === name)!.props
    try {
      render()
      field('bot-name').onChange!({ target: { value: pending.name } })
      field('bot-instructions').onChange!({ target: { value: pending.instructions } })
      ;(render().find(({ type }) => type === 'Select')!.props.onValueChange as (value: string) => void)('arc:' + projectKey(pending.project))
      if (hidden) { open = false; render() }
      workspace.pendingBot = pending
      render(); render()
      // The real refresh updates bots and clears pending in the same batch. A rejected check clears pending without a match.
      workspace.bots = [{ bot_id: confirmed ? pending.bot_id : 'different-bot', name: pending.name }]
      workspace.pendingBot = null
      render(); render()
      assert.equal(open, !hidden, 'background reconciliation only resets fields and never changes drawer focus')
      if (hidden) { open = true; render() }
      assert.equal(field('bot-name').value, confirmed ? '' : pending.name)
      assert.equal(field('bot-instructions').value, confirmed ? '' : pending.instructions)
      assert.equal(field('bot-name').readOnly, false)
      assert.equal(render().find(({ type }) => type === 'Select')!.props.value, confirmed ? 'arc:' : 'arc:' + projectKey(pending.project))
      render().find(({ type }) => type === 'form')!.props.onSubmit!({ preventDefault() {} })
      await settleComponent()
      assert.equal(payloads.length, confirmed ? 0 : 1, 'a confirmed Bot cannot be resubmitted under a fresh ID; proven rejected content remains reusable')
      field('bot-name').onChange!({ target: { value: '新草稿' } })
      workspace.bots = [...workspace.bots]
      render()
      assert.equal(field('bot-name').value, '新草稿', 'later list refreshes preserve the next unsent draft')
    } finally { app.close() }
  })
})


test("bot drafts survive a new connection while old create completion cannot close or unlock its form", async () => {
  let generation = 1, work = pendingResult()
  const closed: boolean[] = []
  const workspace = () => { const owner = generation; return { ...historyActions, generation: owner, isCurrent: () => owner === generation, nodes: [], pendingBot: null, writable: true, createBot: () => work.promise } }
  let currentWorkspace = workspace()
  const app = componentHarness('./bot-form.tsx', { '@tanstack/react-router': { useNavigate: () => () => {} }, '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message } })
  const render = () => app.render('BotForm', { open: true, onOpenChange: (open: boolean) => closed.push(open), restoreFocus() {} })
  try {
    render(); let elements = render()
    elements.find(({ props }) => props.name === 'bot-name')?.props.onChange?.({ target: { value: 'keep my draft' } })
    const submit = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: { reportValidity: () => true } })
    submit(); assert.equal(render().find(({ props }) => props.type === 'submit')?.props.disabled, true)
    const old = work
    generation++; currentWorkspace = workspace(); render(); elements = render()
    assert.equal(elements.find(({ props }) => props.name === 'bot-name')?.props.value, 'keep my draft')
    assert.equal(elements.find(({ props }) => props.type === 'submit')?.props.disabled, false)
    work = pendingResult(); submit()
    old.reject(new Error('stale create error')); await settleComponent()
    assert.equal(render().find(({ props }) => props.type === 'submit')?.props.disabled, true)
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, '')
    assert.deepEqual(closed, [])
    work.resolve({}); await settleComponent()
    assert.deepEqual(closed, [false])
  } finally { app.close() }
})


test("conversation keeps unknown and newly edited drafts across connection changes", async () => {
  let generation = 1, work = pendingResult()
  const workspace = () => { const owner = generation; return {
    ...historyActions, generation: owner, isCurrent: () => owner === generation, conversationId: 'chat', fresh: new Set(), data: context, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: () => work.promise, run: async (action: () => Promise<void>) => { try { await action() } catch {} },
  } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const composer = () => app.render('ConversationScreen').find(({ type }) => type === 'PromptInput')?.props
  try {
    composer()?.onChange?.('keep original draft'); composer()?.onSubmit?.()
    work.reject(new Error('unknown')); await settleComponent()
    assert.equal(composer()?.value, 'keep original draft')
    work = pendingResult(); composer()?.onSubmit?.()
    generation++; currentWorkspace = workspace(); composer()
    work.resolve(true); await settleComponent()
    assert.equal(composer()?.value, 'keep original draft')
    work = pendingResult(); composer()?.onSubmit?.(); composer()?.onChange?.('newer unsent draft')
    work.resolve(true); await settleComponent()
    assert.equal(composer()?.value, 'newer unsent draft')
    work = pendingResult(); composer()?.onSubmit?.(); work.resolve(true); await settleComponent()
    assert.equal(composer()?.value, '')
  } finally { app.close() }
})


test("an old action completion cannot clear a new generation's busy state", async () => {
  let generation = 1
  const old = pendingResult(), fresh = pendingResult()
  const app = componentHarness('./present.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ generation }) } })
  const render = (action: () => Promise<unknown>) => app.render('ActionButton', { children: 'check', action }).find(({ type }) => type === 'Button')?.props
  try {
    render(() => old.promise)?.onClick?.()
    assert.equal(render(() => old.promise)?.disabled, true)
    generation++
    assert.equal(render(() => fresh.promise)?.disabled, false)
    render(() => fresh.promise)?.onClick?.()
    old.resolve(undefined); await settleComponent()
    assert.equal(render(() => fresh.promise)?.disabled, true)
    fresh.resolve(undefined); await settleComponent()
    assert.equal(render(() => fresh.promise)?.disabled, false)
  } finally { app.close() }
})


test("connection form keeps current errors and busy ownership when an earlier connection is cancelled", async () => {
  let generation = 1, work = pendingResult(), synchronousFailure = false
  const device = {
    connectionGeneration: () => generation, entries: [{ kind: 'gateway', address: 'https://node.example', device_id: 'target' }], initialInvitation: '',
    identity: { deviceId: 'browser' }, phase: 'disconnected', error: '', entryName: () => '',
    connect(_entry: unknown, rebind: GenerationAction['rebind']) {
      if (synchronousFailure) throw new Error('请填写有效的访问地址。')
      rebind({ from: generation, to: ++generation })
      return work.promise
    },
  }
  const app = componentHarness('../identity/connection-screen.tsx', { '@vibeface/client/identity/context.tsx': { useDevice: () => device, notJoined: (self: string) => `${self}还没加入`, removedText: (self: string) => `${self}已被移除` }, '@/lib/platform': { webPlatform: { self: '这个浏览器', transport: { connectDevice: async () => { throw new Error('offline') } } } }, '@vibeface/client/identity/connection-lane.ts': { primaryLane }, '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message, entryAddress, entryKey }, '@vibeface/client/workspace/labels.ts': { addressName: (address: string) => address, hostOf: () => '' } })
  const render = () => app.render('ConnectionScreen')
  const connect = () => render().find(({ type, props }) => type === 'Button' && props.title === 'https://node.example')?.props
  try {
    render(); render(); connect()?.onClick?.()
    assert.equal(connect()?.disabled, true)
    const old = work
    generation++; render(); assert.equal(connect()?.disabled, false)
    work = pendingResult(); connect()?.onClick?.()
    old.reject(new Error('stale connection error')); await settleComponent()
    assert.equal(connect()?.disabled, true)
    assert.equal(render().some(({ props }) => props.error), false)
    work.resolve(undefined); await settleComponent()
    assert.equal(connect()?.disabled, false)
    synchronousFailure = true; connect()?.onClick?.(); await settleComponent()
    assert.equal(render().find(({ props }) => props.error)?.props.error, '请填写有效的访问地址。')
  } finally { app.close() }
})
