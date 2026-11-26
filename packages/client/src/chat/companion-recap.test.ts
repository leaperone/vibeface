import assert from 'node:assert/strict'
import test from 'node:test'
import { botRecap, routineState } from './companion-recap.ts'
import { completeHistory } from '../workspace/history-fixture.ts'
import { parseConversation } from '../workspace/model.ts'
import type { InboxItem } from '../workspace/inbox.ts'
import type { InputTarget, Question, WorkspaceDomainState, WorkspaceTask } from '../workspace/types.ts'

const bot = { bot_id: 'bot', conversation_id: 'chat' }
const task = (task_id: string, state: WorkspaceTask['state']): WorkspaceTask => ({
  task_id, root_task_id: task_id, owner_bot: bot.bot_id, title: task_id, request: '请求', expected_result: '结果',
  source_events: [], dependencies: [], reply_only: false, revision: 1, action: 'continue', request_event: 'request',
  event_hash: task_id, created_at_ms: 1, updated_at_ms: 1, state,
})
const workspace = (tasks: WorkspaceTask[] = []): WorkspaceDomainState => ({ tasks, channels: [], inputs: [], assignments: [], coverage: { limit: 20, truncated: false } })
const snapshot = (extra: Record<string, unknown> = {}) => parseConversation({
  conversation: { conversation_id: 'chat', owner: 'node', owner_epoch: 1, title: '' }, history_authority: 'authority', history: completeHistory(),
  messages: [], works: [], deliveries: [], checks: [], local_node: 'node', changes: [], overwritten: [],
  scheduler: { jobs: [], scope_jobs: [], unanswered: [] }, questions: [], sessions: [], sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 },
  bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, ...extra,
}, 'chat')
const question = (question_id: string, target: InputTarget): Question => ({
  question_id, conversation_id: 'chat', target, bot: { bot_id: 'bot', name: 'Bot' }, severity: 'attention', text: '选择发布渠道？', choices: [],
  about: { node: 'node', agent: 'codex', session_id: 'session', cwd: '', summary: '' }, author: 'node', created_at_ms: 1, answer: null, closed: null,
})
const inboxQuestion = (item: Question): InboxItem => ({ kind: 'question', key: item.question_id, title: item.text, done: false, question: item })
const base = () => ({ bot, data: snapshot(), inbox: [], notifications: [], ignoredNotifications: new Set<string>(), workspaceState: workspace(), openWorks: [], fresh: new Set<string>(), connected: true } satisfies Parameters<typeof botRecap>[0])

test('return recap orders decisions, results and live work; task uncertainty never reads as completion', () => {
  const tasks = [task('active', 'running'), task('unknown', 'needs_reconciliation'), task('blocked', 'waiting_user')]
  const result = { notification_id: 'result', conversation_id: 'chat', kind: 'manager_response', text: '构建报告已写好', target: { kind: 'task', task_id: 'done' }, focus: 'report' } satisfies Parameters<typeof botRecap>[0]['notifications'][number]
  const input = { ...base(), workspaceState: workspace(tasks), notifications: [result, result] }
  const rows = botRecap(input)
  assert.deepEqual(rows.map(row => row.group), ['decision', 'decision', 'result', 'working'])
  assert.equal(rows.find(row => row.text === 'unknown')?.state, '结果待确认')
  assert.equal(rows.find(row => row.text === '构建报告已写好')?.notificationId, 'result')
  assert.deepEqual(botRecap({ ...input, workspaceState: workspace([...tasks].reverse()) }), rows)
})

test('a question, its task and the underlying work appear once; two actual questions remain distinct', () => {
  const data = snapshot({ works: [{
    work_id: 'work', target: { kind: 'task', task_id: 'release' }, trigger: 'task:release', target_node: 'node', cwd: '',
    title: '底层工作', prompt: '执行', state: 'unverified', summary: '', outcome: null, result_text: '', failure_reason: null,
    dispatcher: 'node', call_ref: 'call', continues: null, parent: null, thread: null, phase: null, resolved: { provider: 'codex', model: null },
  }] })
  const first = inboxQuestion(question('first', { kind: 'task', task_id: 'release' }))
  const input = { ...base(), data, workspaceState: workspace([task('release', 'waiting_user')]), inbox: [first] }
  assert.equal(botRecap(input).length, 1)
  assert.equal(botRecap(input)[0].focus, 'question:first')
  assert.equal(botRecap({ ...input, inbox: [first, inboxQuestion(question('second', { kind: 'task', task_id: 'release' }))] }).length, 2)
})

test('routine questions retain the precise original target and notice; unlocatable device questions stay global', () => {
  const target = { kind: 'routine', bot_id: 'bot', trigger: 'schedule:daily:1' } satisfies InputTarget
  const original = inboxQuestion(question('routine-question', target))
  const notice = { notification_id: 'original-notice', conversation_id: 'chat', kind: 'question', text: '选择？', target, focus: 'question:routine-question' }
  const rows = botRecap({ ...base(), inbox: [original], notifications: [notice] })
  assert.equal(rows.length, 1)
  assert.deepEqual(rows[0].target, target)
  assert.equal(rows[0].notificationId, 'original-notice')
  assert.deepEqual(botRecap({ ...base(), inbox: [original] }), [])
  const device = question('device', { kind: 'bot_main', bot_id: 'bot' })
  delete device.target
  assert.deepEqual(botRecap({ ...base(), inbox: [inboxQuestion(device)] }), [])
})

test('answered and closed questions disappear without acknowledging or approving anything', () => {
  const original = question('answered', { kind: 'task', task_id: 'task' })
  const done = inboxQuestion({ ...original, answer: { message_id: 'answer', text: '已经选过', author: 'other', created_at_ms: 2 } })
  const closed = inboxQuestion({ ...original, question_id: 'closed', closed: 'request_lost' })
  assert.deepEqual(botRecap({ ...base(), inbox: [done, closed] }), [])
})

test('old loaded history is absent without unread evidence; a foreign conversation cannot leak into a Bot recap', () => {
  const data = snapshot({ messages: [{ message_id: 'old', event_hash: 'old-event', role: 'assistant', text: '旧结果', created_at_ms: 1, status: 'completed' }] })
  assert.deepEqual(botRecap({ ...base(), data }), [])
  assert.deepEqual(botRecap({ ...base(), data, bot: { bot_id: 'other', conversation_id: 'other-chat' }, fresh: new Set(['old-event']) }), [])
})

test('unknown main receipts outrank a completed-looking reply and cache statuses never claim current completion', () => {
  const data = snapshot({
    messages: [{ message_id: 'reply', event_hash: 'reply-event', role: 'assistant', text: '完成', created_at_ms: 1, status: 'completed' }],
    scheduler: { jobs: [{ trigger: 'message:input', scope: { kind: 'main' }, state: 'unknown', message_id: 'input', reply_message_id: 'reply' }], scope_jobs: [], unanswered: [] },
  })
  const rows = botRecap({ ...base(), data, fresh: new Set(['reply-event']), connected: false })
  assert.ok(rows.every(row => row.group === 'decision' && row.state === '结果待确认'))
  assert.ok(rows.every(row => !row.state.includes('已完成')))
  const cache = botRecap({ ...base(), data: snapshot({ messages: data.messages }), fresh: new Set(['reply-event']), connected: false })
  assert.equal(cache[0].state, '最近回复')
})

test('an active routine has one original scope before it has a result notification; offline execution stays uncertain', () => {
  const data = snapshot()
  const job = { conversation_id: 'chat', trigger: 'schedule:daily:1', scope: { kind: 'routine', trigger: 'schedule:daily:1' }, state: 'running', sequence: 1, kind: 'schedule', lane: 'schedule_routine', source_node: 'node', owner: 'node', message_id: null, reply_message_id: 'future' } satisfies NonNullable<Parameters<typeof botRecap>[0]['data']>['scopeJobs'][number]
  data.scopeJobs = [job, job]
  const rows = botRecap({ ...base(), data })
  assert.equal(rows.length, 1)
  assert.deepEqual(rows[0].target, { kind: 'routine', bot_id: 'bot', trigger: job.trigger })
  assert.equal(rows[0].group, 'working')
  assert.equal(routineState(data, job.trigger, false), '状态待确认')
  data.scopeJobs = [{ ...job, state: 'completed' }]
  assert.equal(routineState(data, job.trigger, true), '本轮已结束')
  assert.equal(routineState(data, 'schedule:other:1', true), null)
})
