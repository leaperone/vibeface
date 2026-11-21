
import assert from "node:assert/strict"
import test from "node:test"



import { parseConversation, parseNodes } from "@vibeface/client/workspace/model.ts"
import { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, settledAnswer, sharedProject, questionParts } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"


import { context, componentHarness, pendingResult, componentText, parityShared, settleComponent } from './entry-url-test-kit.test-support.ts'


test("Bot filtering keeps the remaining Work and leaves its device confirmation to 待处理", () => {
  const bots = [{ bot_id: "a", conversation_id: "chat-a", name: "Bot A" }, { bot_id: "b", conversation_id: "chat-b", name: "Bot B" }]
  const openWork = (conversation_id: string) => ({ work_id: `work-${conversation_id}`, conversation_id, trigger: "message:one", target_node: "offline", cwd: "/project", title: conversation_id, prompt: "检查", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, kind: "codex", thread_id: `thread-${conversation_id}` })
  const works = [openWork("chat-a"), openWork("chat-b")]
  const inbox = [{ key: "device", kind: "confirm", check: { kind: "device", check_id: "device", detail: "", target_node: "offline", reason: "offline", works: works.map((work) => ({ kind: "work", check_id: `work:${work.work_id}`, detail: "", work_id: work.work_id, conversation_id: work.conversation_id, target_node: work.target_node, title: work.title, reason: "offline" })) } }]
  const workspace = { bots, selectedBot: bots[1], openWorks: works, nodes: [{ device_id: "offline", name: "离线设备", local: false, online: false, revoked: false }], questions: [], liveProgress: [], connectionOnline: true, synced: true, answered: {}, workspaceState: workspaceModel.parseWorkspaceState({
    channels: [], inputs: [], coverage: { limit: 128, truncated: false },
    tasks: [{ task_id: "task-a", root_task_id: "task-a", owner_bot: "a", title: "Bot A task", request: "request", expected_result: "result", source_events: [], dependencies: [], reply_only: false, revision: 1, action: "continue", request_event: "request", event_hash: "task-a-event", created_at_ms: 1, updated_at_ms: 1, state: "running" }],
    assignments: [{ assignment_id: "assignment-a", task_id: "task-a", started_revision: 1, bot_id: "a", execution_node: "offline", bot_owner_epoch: 1, work_id: "work-chat-b", basis: {}, resource_roots: [], automatic: false, state: "accepted", event_hash: "assignment-a-event", created_at_ms: 1 }],
  }) }
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
  try {
    let elements = app.render("TasksOverview")
    const selectBot = elements.find(({ type }) => typeof type === "function" && type.name === "TaskFilters")?.props.onBotFilter
    assert.ok(typeof selectBot === "function")
    selectBot("b")
    elements = app.render("TasksOverview")
    assert.deepEqual(elements.filter(({ type }) => typeof type === "function" && type.name === "TaskOverviewCard").map(({ props }) => (props.work as { conversation_id: string }).conversation_id), [], "an assignment owned by another Bot still suppresses its selected execution row")
    assert.equal(elements.some(({ type }) => type === "ConfirmCard"), false)
  } finally { app.close() }
})


test('bound permission choices show readable actions and return their original signed labels on both platforms', async () => {
  for (const platform of ['web', 'native']) for (const selected of ['allow_once', 'deny']) {
    const labels = { allow_once: '{"decision":"accept"}', deny: '{"decision":"decline"}' }
    const question = workspaceModel.parseQuestions({ questions: [{ question_id: 'permission', conversation_id: 'chat', target: { kind: 'work', work_id: 'work' }, bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '是否允许本次读取？', choices: [{ label: labels.allow_once, action: 'allow_once' }, { label: labels.deny, action: 'deny' }], about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'held-request' }, author: 'worker', created_at_ms: 1, answer: null, closed: null }] })[0]
    assert.ok(question)
    const sent: string[] = [], request = pendingResult()
    const shared = { ...parityShared(), pendingAnswers: [] as { question_id: string; text: string }[], answer: async (_question: unknown, value: string) => { sent.push(value); return request.promise } }
    const app = componentHarness(platform === 'web' ? './question-card.tsx' : '../../../../../apps/mobile/src/features/chat/question-card.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@/data/context': { useWorkspace: () => ({ members: [], bots: shared.bots, actions: { answer: shared.answer } }) },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'client' } } }) },
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/lib/errors.ts': errors,
      '@/lib/glossary': { terms: { approval: { icon: 'Hand' } } },
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, run: () => Promise<void>) => { try { await run() } catch {} } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      'expo-haptics': { notificationAsync: async () => {}, NotificationFeedbackType: { Success: 'success' } },
    })
    const render = () => app.render('QuestionCard', { question })
    const readable = selected === 'allow_once' ? '允许本次' : '拒绝'
    const raw = selected === 'allow_once' ? labels.allow_once : labels.deny
    const press = () => {
      const button = render().find(({ type, props }) => type === 'Button' && (props.accessibilityLabel === readable || componentText(props.children) === readable))
      assert.ok(button)
      if (platform === 'web') { assert.ok(button.props.onClick); button.props.onClick() }
      else { assert.ok(typeof button.props.onPress === 'function'); button.props.onPress() }
    }
    try {
      const elements = render()
      assert.match(componentText(elements), /允许本次/)
      assert.match(componentText(elements), /拒绝/)
      assert.doesNotMatch(componentText(elements), /decision|accept|decline/)
      assert.equal(elements.some(({ type }) => type === 'Textarea' || type === 'TextInput'), false)
      assert.doesNotMatch(componentText(elements), /忽略/, 'a held approval takes only its own actions')
      shared.writable = false; press(); assert.equal(sent.length, 0)
      shared.writable = true; press(); assert.deepEqual(sent, [raw])
      shared.pendingAnswers = [{ question_id: question.question_id, text: raw }]
      request.reject(new Error('connection lost')); await settleComponent()
      assert.match(componentText(render()), new RegExp(readable))
      assert.doesNotMatch(componentText(render()), /decision|accept|decline/)
      shared.pendingAnswers = []
      shared.answered = { [question.question_id]: raw }
      assert.match(componentText(render()), selected === 'allow_once' ? /已回答 · 允许本次/ : /已拒绝/)
      assert.doesNotMatch(componentText(render()), /decision|accept|decline/)
    } finally { app.close() }
  }
})


test("question cards send a choice on tap, send written text from the last line, ignore on demand, and keep unknown retries and winning answers", async () => {
  const sent: unknown[][] = [], notices: string[] = []
  let actionBusy: string | null = null, answerStatus = 'answered'
  const workspace = { generation: 1, writable: true, nodes: [], answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], announce: (notice: string) => { notices.push(notice) },
    bots: [{ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot' }], openWorks: [], data: { works: [{ work_id: 'w', thread_id: 's', title: '升级依赖', outcome: 'completed' }] },
    answer: async (...args: unknown[]) => { sent.push(args); return { status: answerStatus, text: answerStatus === 'answered' ? args[1] : '停下' } } }
  const toasts: { text: string; action?: { label: string; onClick(): void } }[] = [], visited: unknown[] = []
  const kit = new Proxy({ useAction: () => ({ busy: actionBusy, error: null, run: (_key: string, work: () => Promise<void>) => work() }), toast: (text: string, options: { action?: { label: string; onClick(): void } } = {}) => { toasts.push({ text, action: options.action }) } }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const router = { Link: 'Link', useNavigate: () => (target: unknown) => { visited.push(target) } }
  const app = componentHarness('./question-card.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'web', source: 'membership', can_manage: false } } }) }, '@vibeface/client/workspace/labels.ts': { nodeName: () => '工作电脑' }, '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject }, '@/components/arc/product-kit': kit, '@tanstack/react-router': router })
  const longChoice = '只在本机验证完整配置并保留现有服务，检查通过后再决定是否发布'
  const question = { target: { kind: 'bot_main', bot_id: 'bot' }, question_id: 'q1', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '\n是否允许继续？\n已完成本机检查。\n\n线上结果未知。', choices: [{ label: '允许' }, { label: longChoice }, { label: '停下' }], about: { node: 'worker', agent: 'codex', session_id: 's', cwd: '/path/to/web/', summary: '改登录' }, author: 'gateway', created_at_ms: 1, answer: null, closed: null }
  const render = (props: object = {}) => app.render('QuestionCard', { question, ...props })
  const tap = async (label: string, props: object = {}) => {
    const button = render(props).find(({ type, props }) => (type === 'Button' || type === 'Button') && (props['aria-label'] === label || componentText(props.children).includes(label)))
    assert.ok(button?.props.onClick, `missing button ${label}`)
    button.props.onClick(); await settleComponent()
  }
  const write = (value: string) => {
    const input = render().find(({ type }) => type === 'Textarea')
    assert.ok(input?.props.onChange)
    input.props.onChange({ target: { value } })
  }
  const plain = (value: unknown) => JSON.parse(JSON.stringify(value))
  try {
    let elements = render()
    assert.match(componentText(elements), /web[^]*工作电脑/, 'the context identifies its project and device')
    assert.deepEqual(elements.filter(({ type, props }) => type === 'Button' && props.className === 'w-full justify-start whitespace-normal').map(({ props }) => componentText(props.children)), ['1允许', `2${longChoice}`, '3停下'], 'one numbered row per real choice')
    const input = elements.find(({ type }) => type === 'Textarea')
    assert.equal(input?.props.rows, 1, 'the last line waits for a written answer')
    assert.equal(input?.props['aria-label'], '你的回答')
    assert.equal(input?.props.onKeyDown, undefined, 'Enter and IME stay native textarea input')
    assert.equal(elements.find(({ type, props }) => type === 'Button' && props['aria-label'] === '发送回答')?.props.disabled, true, 'an empty answer cannot be sent')
    assert.ok(elements.some(({ props }) => props.text === '已完成本机检查。\n\n线上结果未知。'), 'the decision context keeps its paragraphs')
    await tap('允许')
    assert.deepEqual(plain(sent.at(-1)), [question, '允许'], 'a tap sends the exact choice')
    await tap(longChoice)
    assert.deepEqual(plain(sent.at(-1)), [question, longChoice], 'the complete choice is submitted')
    for (const value of ['1', '１', '先检查配置\n确认后再继续']) {
      write(value)
      await tap('发送回答')
      assert.deepEqual(plain(sent.at(-1)), [question, value], 'custom text is literal, including digits and newlines')
    }
    const count = sent.length
    write('   ')
    assert.equal(render().find(({ type, props }) => type === 'Button' && props['aria-label'] === '发送回答')?.props.disabled, true, 'whitespace cannot be submitted')
    write('新回答')
    workspace.writable = false
    elements = render()
    assert.equal(elements.filter(({ type }) => type === 'Button' || type === 'Textarea').every(({ props }) => props.disabled === true), true)
    await tap('停下'); await tap('忽略')
    assert.equal(sent.length, count, 'the send handler also enforces read-only state')
    workspace.writable = true
    await tap('忽略')
    assert.deepEqual(plain(sent.at(-1)), [question, ignoredAnswer], 'ignoring sends the fixed answer')
    workspace.pendingAnswers = [{ question_id: 'q1', text: '继续\n原文' }]
    elements = render()
    assert.equal(elements.some(({ type,props }) => type === 'Button' && props.className === 'w-full justify-start whitespace-normal' || type === 'Textarea'), false, 'a pending answer cannot be replaced')
    assert.match(componentText(elements), /没确认送达/)
    const retry = elements.find(({ type, props }) => type === 'Button' && componentText(props.children).includes('再试'))
    assert.ok(retry?.props.onClick)
    retry.props.onClick(); await settleComponent()
    assert.deepEqual(plain(sent.at(-1)), [question, '继续\n原文'], 'unknown results retry their original text')
    actionBusy = 'retry'
    elements = render()
    assert.match(componentText(elements), /正在发送/)
    assert.doesNotMatch(componentText(elements), /没确认送达/)
    assert.equal(elements.find(({ type }) => type === 'Button')?.props.disabled, true)
    actionBusy = null
    workspace.answered = { q1: '允许' }
    elements = render({ question: { ...question, closed: 'request_lost', answer: { text: '停下' } }, answer: '过时答案' })
    assert.match(componentText(elements), /已由另一端回答：停下/, 'the network winner takes precedence over a folded answer, closed state and pending retry')
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'Button'), false)
    workspace.pendingAnswers = []
    elements = render()
    assert.match(componentText(elements), /已回答 · 允许/, 'a shared answer folds the card immediately')
    assert.doesNotMatch(componentText(elements), /升级依赖/, 'an ordinary question names no held task')
    elements = render({ question: { ...question, about: { ...question.about, request_id: 'r' } } })
    assert.match(componentText(elements), /升级依赖[^]*工作电脑/)
    workspace.answered = { q1: ignoredAnswer }
    assert.match(componentText(render()), /已忽略/, 'an ignored question folds as ignored')
    workspace.answered = {}
    elements = render({ question: { ...question, closed: 'request_lost' } })
    assert.match(componentText(elements), /已失效/)
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'Button'), false)
    await tap('允许', { inbox: true })
    await tap('忽略', { inbox: true })
    assert.deepEqual(toasts.map(({ text }) => text), ['已回答', '已忽略'])
    toasts[0].action?.onClick()
    assert.deepEqual(plain(visited), [{ to: '/b/$botId', params: { botId: 'bot' } }])
    answerStatus = 'already_answered'
    await tap('允许', { inbox: true })
    assert.deepEqual(notices, ['这个问题已经回答过了'], 'the existing race announcement is retained')
    assert.equal(toasts.length, 2, 'a losing answer has no success toast')
  } finally { app.close() }
})


test("a question without choices offers only the answer line and 忽略", () => {
  const app = componentHarness('./question-card.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ writable: true, nodes: [], answered: {}, pendingAnswers: [], bots: [], openWorks: [] }) },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备' },
    '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject },
    '@/components/arc/product-kit': { useAction: () => ({ busy: null, error: null }) },
    '@tanstack/react-router': { useNavigate: () => () => {} },
  })
  try {
    const elements = app.render('QuestionCard', { question: { question_id: 'free', text: '需要什么配置？', choices: [], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: null, closed: null } })
    assert.equal(elements.find(({ type }) => type === 'Textarea')?.props.rows, 1)
    assert.equal(elements.some(({ type,props }) => type === 'Button' && props.className === 'w-full justify-start whitespace-normal'), false)
    assert.ok(elements.some(({ type, props }) => type === 'Button' && componentText(props.children).includes('忽略')))
  } finally { app.close() }
})


test("remounted question cards identify the answer author and reserve warning styling for a lost race", () => {
  for (const platform of ['web', 'mobile']) {
    for (const scenario of [{ author: 'phone', mine: undefined, elsewhere: true, warned: false }, { author: 'gateway', mine: undefined, elsewhere: true, warned: false }, { author: 'self', mine: undefined, elsewhere: false, warned: false }, { author: 'phone', mine: '停下', elsewhere: true, warned: true }, { author: 'phone', mine: '继续', elsewhere: true, warned: false }]) {
      const question = { question_id: 'remount', text: '继续吗？\n线上结果未知。', choices: [{ label: '允许' }], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: { text: '继续', author: scenario.author }, closed: 'request_lost' }
      const shared = { writable: true, nodes: [], answered: scenario.mine === undefined ? {} : { remount: scenario.mine }, pendingAnswers: [{ question_id: 'remount', text: '旧草稿' }], bots: [], openWorks: [] }
      const app = componentHarness(platform === 'web' ? './question-card.tsx' : '../../../../../apps/mobile/src/features/chat/question-card.tsx', {
        '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
        '@/data/context': { useWorkspace: () => ({ members: [], bots: [] }) },
        '@/data/live': { useLive: () => shared },
        '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { principal: { device_id: 'self', source: 'membership', can_manage: false } } }) },
        '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备' },
        '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject },
        '@/components/arc/product-kit': { useAction: () => ({ busy: null, error: null }) },
        '@tanstack/react-router': { useNavigate: () => () => {} },
      })
      try {
        const elements = app.render('QuestionCard', { question, answer: '过时的折叠答案' })
        const settled = elements.find(({ props }) => props.ask === '继续吗？')
        assert.ok(settled, `${platform} returns the standing answer before closed or pending state`)
        assert.equal(componentText(settled.props.children), scenario.elsewhere ? '已由另一端回答：继续' : '已回答 · 继续')
        const marker = settled.props.mark as { props: { className: string } } | undefined
        const className = platform === 'web' ? marker?.props.className : settled.props.className
        assert.equal(String(className).includes('text-warning'), scenario.warned, `${platform}: another author alone is not a failed race`)
        assert.equal(elements.some(({ type }) => type === 'Button' || type === 'input' || type === 'TextInput'), false)
      } finally { app.close() }
    }
  }
})


test("task overview keeps pending decisions with their task and every real task visible", () => {
  const work = {
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    resolved: { provider: 'codex', model: null }, trigger: 'message:one', target_node: 'offline', cwd: '/project', title: '检查项目', prompt: '', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null }
  const approvalRows = [{ target: { kind: 'work', work_id: 'held' }, question_id: 'approval', conversation_id: 'chat', bot: null, severity: 'attention', text: '继续吗？', choices: [{ label: '继续' }], about: { node: 'offline', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' }, author: 'offline', created_at_ms: 1, answer: null, closed: null }]
  const approvalQuestions = workspaceModel.parseQuestions({ questions: approvalRows })
  const data = parseConversation({ ...context, works: [{ ...work, work_id: 'held', thread_id: 'session' }, { ...work, work_id: 'running', target_node: 'online' }], questions: approvalRows })
  const works = data.works.map((item) => ({ ...item, conversation_id: 'chat' }))
  const nodes = parseNodes({ nodes: [{ device_id: 'offline', name: '离线电脑', online: false, local: false, revoked: false }, { device_id: 'online', name: '在线电脑', online: true, local: false, revoked: false }] })
  const overview = workspaceModel.overviewFromWorkspace({ openWorks: works, bots: [], questions: [], nodes })
  const workspace = { nodes, openWorks: works, bots: [], questions: approvalQuestions, overview, selectedBot: null, data, conversationId: 'chat', synced: true, connectionOnline: true, answered: {}, liveProgress: [] }
  const checks = workspaceModel.workChecks(works, nodes)
  const app = componentHarness('./tasks-overview.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/workspace/labels.ts': taskLabels,
    './inbox': { useInbox: () => [{ kind: 'question', done: false, question: approvalQuestions[0] }, ...checks.map((check) => ({ kind: 'confirm', check }))] },
    './workspace-shell': { useCreateBot: () => () => {} },
    '@tanstack/react-router': { Link: 'Link' },
  })
  try {
    const elements = app.render('TasksOverview')
    assert.deepEqual(elements.filter(({ props }) => props.work).map(({ props }) => (props.work as { work_id: string }).work_id), ['held', 'running'], 'a task waiting for confirmation remains visible before active tasks')
    assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === 'TaskRunSummary')?.props.attention, 1, 'the same task, question and confirmation are counted once')
    assert.equal(elements.some(({ type }) => type === 'QuestionCard' || type === 'ConfirmCard'), false, 'the question and confirmation act only in 待处理')
    assert.equal(elements.some(({ props }) => typeof props.children === 'string' && props.children === '2 项任务'), true)
  } finally { app.close() }
})
