import assert from "node:assert/strict"
import type { SessionScope } from './types.ts'
import test from "node:test"
import { parseChanged } from "../lib/progress.ts"
import { parsePrincipal, text } from "../lib/node.ts"
import { botReadiness, confirmReason, overviewFromWorkspace, parseConversation, parseNodes, parseNotifications, parseOpenWorks, parseQuestions, parseRecords, parseScopeContext, parseWorkDetail, parseWorkspaceHistory, parseWorkspaceState, workChecks } from "./model.ts"
import { answerBody } from './questions.ts'
import { mergeBots, mergeConversation, mergeNotifications, mergeOpenWorks, mergeQuestions, mergeWorkspaceState } from "./records.ts"
import { taskAgent } from "./labels.ts"
import type { NodeInfo } from "./types.ts"

const mainScope: SessionScope = { kind: 'main' }

const rank = (lamport: number) => `${String(lamport).padStart(20, "0")}:${"0".repeat(20)}:h${lamport}`
const message = (message_id: string, lamport: number) => ({ message_id, role: "user", text: message_id, event_hash: `e-${message_id}`, created_at_ms: lamport, rank: rank(lamport) })
const work = (work_id: string, extra: Record<string, unknown> = {}) => ({
  dispatcher: 'gateway', call_ref: 'dispatch',
  continues: null, parent: null,
  thread: null, phase: null,
  work_id, trigger: "t", target_node: "worker", cwd: "/w", title: "", prompt: "p", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, kind: "codex", resolved: { provider: "codex", model: null }, ...extra,
})
const question = (question_id: string, conversation_id: string, answer: unknown = null) => ({
  question_id, conversation_id, bot: null, severity: "info", text: "?", choices: [],
  target: { kind: 'bot_main', bot_id: 'bot' },
  about: { node: "n", agent: "codex", session_id: "s", cwd: "/", summary: "" }, author: "n", created_at_ms: 1, answer,
})
const shown = parseConversation({
  conversation: { conversation_id: "chat", title: "", owner: "gateway", owner_epoch: 1 },
  messages: [message("m1", 1), message("m3", 3)], works: [work("w1"), work("w2")],
  deliveries: [{ state: "dispatch_waiting", work_id: "w9", target_node: "worker", cwd: "/w" }, { state: "stop_waiting", work_id: "w1", target_node: "worker" }],
  scheduler: { jobs: [{ scope: mainScope, trigger: "a", state: "queued" }], scope_jobs: [], unanswered: ["m1"] }, checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 },
  questions: [], sessions: [],
}, "chat")
const records = (extra: Record<string, unknown> = {}) => {
  const event = parseChanged({
    type: "records", conversation_id: "chat", messages: [], works: [], last_message: null, questions: [], notifications: [],
    deliveries: [], checks: [], jobs: [], scope_jobs: [], unanswered: [], sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, open_works: [], ...extra,
  })
  assert.equal(event.type, "records")
  return parseRecords(event.type === "records" ? event.value : null)
}

test('response parsers retain their text, numeric and error boundaries', () => {
  const principal = { network_id: 'a'.repeat(64), device_id: 'b'.repeat(64), source: 'membership', can_manage: false, epoch: 1 }
  assert.equal(parsePrincipal(principal).epoch, 1)
  assert.throws(() => parsePrincipal({ ...principal, epoch: 0 }), { message: '设备返回的编号或版本无效。' })
  assert.throws(() => text(''), { message: '文本缺失或格式无效。' })
  assert.equal(parseNotifications({ notifications: [{ notification_id: 'n', conversation_id: 'chat', kind: 'message', text: '' }] })[0].text, '')
  const state = { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 0, truncated: false } }
  assert.equal(parseWorkspaceState(state).coverage.limit, 0)
  assert.throws(() => parseWorkspaceState({ ...state, channels: {} }), { message: '设备返回的内容缺少列表。' })
  assert.throws(() => parseWorkspaceState({ ...state, coverage: { limit: 0, truncated: 'false' } }), { message: '设备返回的内容缺少开关值。' })
  assert.throws(() => parseWorkspaceState([]), { message: '设备返回的内容格式不对。' })
})

test("task labels use the accepted provider and leave summaries unknown", () => {
  const accepted = records({ works: [work("frozen", { kind: "task", resolved: { provider: "claude", model: null } })] }).works[0]
  assert.equal(taskAgent(accepted), "claude")
  assert.equal(taskAgent({ kind: "task" }), null)
  assert.equal(taskAgent({ kind: "pi" }), "pi")
})

test('scoped workspace reads retain sibling tasks without claiming complete global coverage', () => {
  const task = (task_id: string, revision = 1) => ({ task_id, root_task_id: task_id, owner_bot: 'bot', title: task_id, request: '请求', expected_result: '', source_events: [], dependencies: [], reply_only: false, revision, action: 'continue', request_event: 'request', event_hash: `event-${revision}`, created_at_ms: 1, updated_at_ms: revision, state: 'not_started' })
  const snapshot = (tasks: unknown[]) => parseWorkspaceState({ channels: [], tasks, assignments: [], inputs: [], coverage: { limit: 20, truncated: false } })
  const first = mergeWorkspaceState(null, snapshot([task('one')]), true)
  assert.equal(first.coverage.truncated, true, 'a complete task read is still an incomplete workspace')
  const second = mergeWorkspaceState(first, snapshot([task('two'), task('one', 2)]), true)
  const stale = mergeWorkspaceState(second, snapshot([task('one')]), true)
  assert.deepEqual(stale.tasks.map((item) => [item.task_id, item.revision]), [['one', 2], ['two', 1]])
  assert.equal(stale.coverage.truncated, true)
})

test('an Assignment result declaration does not become a verified completed result', () => {
  const assignment = { assignment_id: 'assignment', task_id: 'task', started_revision: 1, bot_id: 'bot', execution_node: 'node', bot_owner_epoch: 1, basis: { author: 4 }, resource_roots: ['/project'], automatic: false, accepted_revision: 1, adopted_revision: 1, completed_revision: null, trigger_message_id: 'input', source_event: 'source', submitted_result: { revision: 2, text: 'Agent声明已完成', event_hash: 'declared-result' }, state: 'adopted', result: null, event_hash: 'assignment-event', created_at_ms: 1 }
  const state = parseWorkspaceState({ channels: [], tasks: [], assignments: [assignment], inputs: [], coverage: { limit: 20, truncated: false } })
  assert.equal(state.assignments[0].state, 'adopted')
  assert.equal(state.assignments[0].result, undefined)
  assert.equal(state.assignments[0].completed_revision, undefined)
  assert.equal(state.assignments[0].submitted_result?.revision, 2)
  assert.deepEqual(state.assignments[0].basis, { author: 4 })
})

test('workspace history validates the exact event kind and verified speaker', () => {
  const body = { type: 'input_posted', message_id: 'm', target: { kind: 'task', task_id: 'one' }, intent: 'continue', sequence: 1, expected_revision: 1, text: '只属于此任务' }
  const event = { event_hash: 'event', created_at_ms: 1, scope_id: 'chat', kind: 'input_posted', body, speaker: { kind: 'human', device_id: 'client' } }
  const history = parseWorkspaceHistory({ events: [event], coverage: { limit: 20, truncated: true, scope: 'task' } })
  assert.equal(history.coverage.truncated, true)
  assert.deepEqual(history.events[0].speaker, { kind: 'human', device_id: 'client' })
  assert.equal(history.events[0].body.type === 'input_posted' && history.events[0].body.target.kind, 'task')
  assert.throws(() => parseWorkspaceHistory({ events: [{ ...event, kind: 'message_posted' }], coverage: history.coverage }))
  assert.throws(() => parseWorkspaceHistory({ events: [{ ...event, speaker: { kind: 'bot', device_id: 'node' } }], coverage: history.coverage }))
  const unavailable = { conversation_id: 'chat', target: body.target, available: false, context: null, reason: 'owner_unreachable' }
  assert.deepEqual(parseScopeContext(unavailable), unavailable)
  assert.throws(() => parseScopeContext({ ...unavailable, available: true }))
})

test('Main snapshots and pushed records exclude Task messages and replies while keeping Main inputs', () => {
  const taskTarget = { kind: 'task', task_id: 'task' }
  const main = { ...message('main', 4), target: { kind: 'bot_main', bot_id: 'bot' } }
  const taskMessages = [{ ...message('task-input', 5), target: taskTarget }, { ...message('task-reply', 6), role: 'assistant', in_reply_to: 'task-input', target: taskTarget }]
  const questions = [question('main-question', 'chat'), { ...question('task-question', 'chat'), target: taskTarget }, { ...question('unknown-question', 'chat'), target: undefined }]
  const update = records({ messages: [main, ...taskMessages], questions, jobs: [{ scope: { kind: 'workspace_task', task_id: 'task' }, trigger: 'task-input', state: 'running' }, { scope: mainScope, trigger: 'main', state: 'queued' }] })
  const next = mergeConversation(shown, update)
  assert.deepEqual(next.messages.map((item) => item.message_id), ['m1', 'm3', 'main'])
  assert.deepEqual(next.scheduler?.jobs.map((item) => item.trigger), ['main'])
  assert.deepEqual(next.questions.map((item) => item.question_id), ['main-question'])
  assert.deepEqual(mergeQuestions([], update).map((item) => item.question_id), ['main-question', 'task-question', 'unknown-question'], 'the global Inbox retains every verified target and unknown scope')
  const snapshot = parseConversation({ ...shown, messages: [main, ...taskMessages], scheduler: { jobs: update.jobs, scope_jobs: update.scope_jobs, unanswered: [] }, deliveries: [] }, 'chat')
  assert.deepEqual(snapshot.messages.map((item) => item.message_id), ['main'])
  assert.deepEqual(snapshot.scheduler?.jobs.map((item) => item.trigger), ['main'])
  assert.throws(() => mergeConversation(shown, { ...update, conversation_id: 'another-chat' }))
})

test('global open works and confirmation cards retain their exact ChannelTask source', () => {
  const target = { kind: 'channel_task', channel_id: 'channel', task_id: 'task', bot_id: 'bot' }
  const works = parseOpenWorks({ works: [{ ...work('channel-work', { state: 'unverified', outcome: 'unverified', target }), conversation_id: 'chat' }] })
  assert.deepEqual(works[0].target, target)
  assert.equal(works[0].prompt, 'p')
  const checks = workChecks(works, [])
  assert.equal(checks.length, 1)
  assert.deepEqual(checks[0].kind === 'work' && checks[0].target, target)
  const update = records({ open_works: works, works: [] })
  assert.deepEqual(mergeOpenWorks([], update).map((item) => item.target), [target])
  assert.equal(mergeConversation(shown, update).works.some((item) => item.work_id === 'channel-work'), false)
})

test('scope_jobs preserves registered Routine metadata without exposing private ManagerJob fields', () => {
  const routine = { conversation_id: 'chat', trigger: 'schedule:routine:1', kind: 'schedule', scope: { kind: 'routine', trigger: 'schedule:routine:1' }, state: 'running', sequence: 2, lane: 'schedule_routine', source_node: 'owner', owner: 'owner', message_id: null, reply_message_id: 'routine-reply', context: { private: 'private-context' }, failure: { detail: 'private-failure' }, partial_text: 'private-stream' }
  const snapshot = parseConversation({ ...shown, deliveries: [], scheduler: { jobs: shown.scheduler?.jobs ?? [], scope_jobs: [routine], unanswered: [] }, sent_coverage: { scope: 'main', omitted_unknown: 2 } }, 'chat')
  assert.equal(snapshot.scopeJobs[0].scope.kind, 'routine')
  assert.equal(snapshot.scopeJobs[0].message_id, null)
  assert.deepEqual(snapshot.scheduler?.jobs.map((job) => job.trigger), ['a'])
  assert(!JSON.stringify(snapshot.scopeJobs).includes('private-'))
  const updated = mergeConversation(snapshot, records({ scope_jobs: [{ ...routine, state: 'queued', sequence: 3 }], sent_coverage: { scope: 'main', omitted_unknown: 3 } }))
  assert.equal(updated.scopeJobs.length, 1)
  assert.equal(updated.scopeJobs[0].sequence, 3)
  assert.equal(updated.scheduler?.scope_jobs[0].state, 'queued')
  assert.equal(updated.sent_coverage.omitted_unknown, 3)
  assert.throws(() => records({ scope_jobs: [{ ...routine, conversation_id: 'another-chat' }] }))
})

test('an exact shared Work detail can include an ended Work while keeping coverage incomplete', () => {
  const detail = parseWorkDetail({ work: { ...work('ended', { state: 'finished', outcome: 'completed', result_text: '已完成的共享结果' }), conversation_id: 'chat', target: { kind: 'task', task_id: 'source-task' }, source_events: ['dispatch-event', 'finished-event'] }, coverage: { source: 'shared_history', complete: false } })
  assert.equal(detail.work.work_id, 'ended')
  assert.equal(detail.work.result_text, '已完成的共享结果')
  assert.deepEqual(detail.work.target, { kind: 'task', task_id: 'source-task' })
  assert.deepEqual(detail.work.source_events, ['dispatch-event', 'finished-event'])
  assert.equal(detail.coverage.complete, false)
  assert.equal(parseWorkDetail({ ...detail, work: { ...detail.work, target: null } }).work.target, null)
  assert.throws(() => parseWorkDetail({ ...detail, coverage: { source: 'native_transcript', complete: true } }))
  assert.throws(() => parseWorkDetail({ ...detail, work: { ...detail.work, source_events: Array.from({ length: 65 }, (_, index) => `event-${index}`) } }))
})

test('native approval choices retain their bound action and answers remain readable labels', () => {
  const choices = [{ label: '允许本次', action: 'allow_once' }, { label: '拒绝', action: 'deny' }]
  const [approval] = parseQuestions({ questions: [{ ...question('native-approval', 'chat'), choices }] })
  assert.deepEqual(approval.choices, choices)
  assert.deepEqual(overviewFromWorkspace({ openWorks: [], bots: [], questions: [approval], nodes: [] }).questions[0].choices, choices)
  const answer = answerBody(approval.question_id, approval.choices[0].label)
  assert.equal(answer.text, '允许本次')
  assert.equal('action' in answer, false)
  const [ordinary] = parseQuestions({ questions: [{ ...question('ordinary', 'chat'), choices: [{ label: '继续' }] }] })
  assert.equal('action' in ordinary.choices[0], false)
  assert.throws(() => parseQuestions({ questions: [{ ...question('invalid', 'chat'), choices: [{ label: '允许全部', action: 'allow_all' }] }] }))
})

test("a pushed message lands in rank order and the push replaces deliveries, checks, jobs and questions", () => {
  const next = mergeConversation(shown, records({
    messages: [message("m2", 2)], works: [work("w2", { state: "finished", outcome: "completed" })],
    jobs: [{ scope: mainScope, trigger: "a", state: "completed" }], questions: [question("q", "chat")],
  }))
  assert.deepEqual(next.messages.map((item) => item.message_id), ["m1", "m2", "m3"])
  assert.deepEqual(next.works.map((item) => [item.work_id, item.outcome, item.stop]), [["w1", null, undefined], ["w2", "completed", undefined]], "no delivery is still carried")
  assert.deepEqual(next.scheduler?.jobs.map((item) => item.state), ["completed"])
  assert.deepEqual(next.scheduler?.unanswered, [], "a reply queued here clears the unanswered card")
  assert.equal(next.questions.length, 1)
  const again = mergeConversation(next, records({ messages: [message("m2", 2)] }))
  assert.equal(again.messages.length, 3, "a repeated push does not duplicate the message")
})

test("the owner's jobs stay through a push of this device's records and replace its undelivered copy", () => {
  const away = parseConversation({
    conversation: { conversation_id: "chat", title: "", owner: "owner", owner_epoch: 1 },
    messages: [], works: [], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, questions: [], sessions: [],
    scheduler: { jobs: [{ scope: mainScope, trigger: "message:m1", state: "running", source_node: "owner" }], scope_jobs: [], unanswered: [], remote: { owner: "owner", reachable: true } },
  }, "chat")
  assert.deepEqual(away.scheduler?.remote, { owner: "owner", reachable: true })
  const next = mergeConversation(away, records({ jobs: [{ scope: mainScope, trigger: "message:m1", state: "pending_delivery" }, { scope: mainScope, trigger: "message:m2", state: "pending_delivery" }] }))
  assert.deepEqual(next.scheduler?.jobs.map((item) => [item.trigger, item.state]), [["message:m2", "pending_delivery"], ["message:m1", "running"]])
  const local = mergeConversation(shown, records({ jobs: [{ scope: mainScope, trigger: 'a', state: 'running', source_node: 'gateway' }] }))
  const finished = mergeConversation(local, records({ jobs: [{ scope: mainScope, trigger: 'a', state: 'completed', source_node: 'gateway' }] }))
  assert.deepEqual(finished.scheduler?.jobs.map((item) => [item.trigger, item.state]), [['a', 'completed']], 'local observations replace earlier local jobs')
})

test("context state and queue reasons survive records without replacing a remote owner's context", () => {
  const context = {
    scope: mainScope,
    sources: [{ kind: 'message', event_id: 'source', author: 'client', seq: 1, reference: 'm1', prompt: 'private-source-prompt' }], sources_omitted: 2,
    inputs: [{ message_id: 'm2', source_event: 'input-source', stage: 'provider_accepted', actual_turn_id: null, expected_turn_id: 'expected-turn', accepted_at_ms: 1 }], inputs_omitted: 1,
    attempt_id: 'attempt', provider: 'codex', stage: 'compiled', session_generation: 2,
    stable_fingerprint: 'a'.repeat(64), dynamic_fingerprint: 'b'.repeat(64),
    stable_bytes: 100, dynamic_bytes: 200, invalidation_reason: 'stable_prompt_or_provider_changed',
    coverage: { messages_omitted: 3, works_omitted: 0, dispatches_omitted: 1, questions_omitted: 0, attachments_omitted: 0, memory_omitted: 2, checkpoint_omitted: 1, handoff_omitted: 0, pending_checks_omitted: 2, bot_indexes_omitted: { skills: 3 }, history_retrieval: 'context' },
    session_status: { resumable: true, pending_generation: true, native_id: 'private-native-id' },
    native_token: 'must-not-enter-client-state', rendered: 'private context',
  }
  const conflict = { call_ref: 'remember', memory_id: 'memory', expected_epoch: 1, actual_epoch: 1, expected_revision: '', actual_revision: 'new', reason: 'revision_changed' }
  const memory = { memory_id: 'memory', text: '已保存的事实', metadata: { epoch: 1, revision: 'c'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: false, source_event_hash: 'd'.repeat(64), native_token: 'private-memory-token' } }
  const sync = { items: [{ memory_id: 'memory', losing_revision: 'a'.repeat(64), winning_revision: 'c'.repeat(64), losing_event_hash: 'b'.repeat(64), winning_event_hash: 'd'.repeat(64), reason: 'user_protected', text: 'private-conflict-text', signature: 'private-conflict-signature' }], has_more: true }
  const update = records({ context, memory: [memory], memory_staged_count: 2, memory_conflicts: [conflict], memory_sync_conflicts: sync, jobs: [{ scope: mainScope, trigger: 'a', state: 'queued', lane: 'main', hold_reason: 'active_turn', pending_check: false, context }] })
  const next = mergeConversation(shown, update)
  assert.equal(next.scheduler?.context?.stage, 'compiled')
  assert.deepEqual(next.scheduler?.context?.scope, mainScope)
  assert.equal(next.scheduler?.context?.inputs[0].stage, 'provider_accepted')
  assert.equal(next.scheduler?.context?.inputs[0].actual_turn_id, null)
  assert.equal(next.scheduler?.context?.sources_omitted, 2)
  assert(!JSON.stringify(next).includes('private-source-prompt'))
  const taskContext = parseScopeContext({ conversation_id: 'chat', target: { kind: 'task', task_id: 'task' }, available: true, context: { ...context, scope: { kind: 'workspace_task', task_id: 'task' } }, reason: null })
  assert.equal(taskContext.available && taskContext.context.scope.kind, 'workspace_task')
  assert.throws(() => parseScopeContext({ conversation_id: 'chat', target: { kind: 'task', task_id: 'task' }, available: true, context, reason: null }))
  assert.deepEqual(mergeConversation(next, records({ context: { ...context, scope: { kind: 'workspace_task', task_id: 'task' } } })).scheduler?.context?.scope, mainScope)
  assert.equal(next.scheduler?.context?.coverage.messages_omitted, 3)
  assert.deepEqual(next.scheduler?.context?.coverage, context.coverage)
  assert.deepEqual(next.scheduler?.context?.session_status, { resumable: true, pending_generation: true })
  assert.equal(next.scheduler?.jobs[0].lane, 'main')
  assert.equal(next.scheduler?.jobs[0].hold_reason, 'active_turn')
  assert.equal(next.scheduler?.jobs[0].pending_check, false)
  assert.equal(next.scheduler?.jobs[0].context?.session_generation, 2)
  assert.deepEqual(next.bot_settings.memory_conflicts, [conflict])
  assert.equal(next.bot_settings.memory_staged_count, 2)
  assert.equal(next.bot_settings.memory_sync_conflicts.items[0].reason, 'user_protected')
  assert.equal(next.bot_settings.memory_sync_conflicts.has_more, true)
  assert.deepEqual(next.bot_settings.memory[0].metadata, { epoch: 1, revision: 'c'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: false, source_event_hash: 'd'.repeat(64) })
  assert(!JSON.stringify(next).includes('must-not-enter-client-state'))
  assert(!JSON.stringify(next).includes('private context'))
  assert(!JSON.stringify(next).includes('private-native-id'))
  assert(!JSON.stringify(next).includes('private-memory-token'))
  assert(!JSON.stringify(next).includes('private-conflict-text'))
  assert(!JSON.stringify(next).includes('private-conflict-signature'))
  const away = { ...next, scheduler: next.scheduler && { ...next.scheduler, remote: { owner: 'owner', reachable: false } } }
  const local = records({ context: { ...context, stage: 'failed', session_generation: 3 }, memory: [], memory_sync_conflicts: { items: [], has_more: false }, memory_staged_count: 0, memory_conflicts: [] })
  const remote = mergeConversation(away, local)
  assert.equal(remote.scheduler?.context?.session_generation, 2)
  assert.equal(remote.bot_settings.memory_staged_count, 2)
  assert.deepEqual(remote.bot_settings.memory_conflicts, [conflict])
  assert.deepEqual(remote.bot_settings.memory_sync_conflicts, { items: [], has_more: false }, 'gateway sync history updates even when the owner cannot answer')
  assert.deepEqual(mergeConversation(away, records({ memory_conflicts: null })).bot_settings.memory_sync_conflicts, next.bot_settings.memory_sync_conflicts, 'an absent sync page never replaces the last gateway page with owner observations')
  assert.throws(() => records({ memory_sync_conflicts: null }))
  assert.throws(() => records({ memory_sync_conflicts: { ...sync, has_more: 'yes' } }))
  assert.throws(() => records({ memory_sync_conflicts: { ...sync, items: [{ ...sync.items[0], reason: 'unknown' }] } }))
  assert.throws(() => records({ memory_sync_conflicts: { ...sync, items: Array.from({ length: 51 }, () => sync.items[0]) } }))
  assert.deepEqual(remote.bot_settings.memory, [], 'DAG memory changes still update through this gateway')
  assert.equal(mergeConversation(next, records({ context: null })).scheduler?.context, null)
  assert.throws(() => records({ context: { ...context, stage: 'invented' } }))
  assert.throws(() => records({ context: { ...context, inputs: [{ ...context.inputs[0], stage: 'read' }] } }))
  assert.throws(() => records({ context: { ...context, stable_fingerprint: 'invalid' } }))
  assert.throws(() => records({ context: { ...context, session_status: { resumable: 'yes', pending_generation: false } } }))
  assert.throws(() => records({ context: { ...context, coverage: { ...context.coverage, bot_indexes_omitted: { skills: -1 } } } }))
  assert.throws(() => records({ memory_staged_count: -1 }))
  assert.throws(() => records({ memory: [{ ...memory, metadata: { ...memory.metadata, origin: 'unknown' } }] }))
  assert.equal(mergeConversation(next, records({ memory_staged_count: null, memory_conflicts: null })).bot_settings.memory_staged_count, null)
  for (const hold_reason of ['provider_serial', 'task_limit', 'machine_slot', 'pending_check', 'backend_config_pending', 'attachment_preparation', 'earlier_input', 'resource_conflict']) assert.equal(records({ jobs: [{ scope: mainScope, trigger: 'a', state: 'queued', hold_reason }] }).jobs[0].hold_reason, hold_reason)
})

test('a task coordination queue stays separate from its shared work state', () => {
  const [task] = records({ works: [work('task', { kind: 'task', state: 'running', coordination_job: { conversation_id: 'chat', kind: 'agent_task', scope: { kind: 'task', work_id: 'task' }, trigger: 'task:task', state: 'queued', sequence: 1, lane: 'task', source_node: 'gateway', owner: 'gateway', message_id: null, reply_message_id: 'reply', hold_reason: 'machine_slot', native_token: 'private-task-token' } })] }).works
  assert.equal(task.state, 'running')
  assert.equal(task.coordination_job?.state, 'queued')
  assert.equal(task.coordination_job?.hold_reason, 'machine_slot')
  assert(!JSON.stringify(task).includes('private-task-token'))
})

test('unverified provider readiness is preserved without claiming setup is required', () => {
  const nodes = parseNodes({ nodes: [{ device_id: 'node', name: '', online: true, local: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'unverified', relay: false, roots: [] } }] })
  assert.equal(botReadiness(nodes[0]), 'unverified')
})

test("pushed records update the network-wide lists for their conversation only", () => {
  const pushed = records({
    conversation_id: "chat", questions: [question("open", "chat"), question("done", "chat", { message_id: "a", text: "好", author: "b", created_at_ms: 2 }),
      { ...question("lost", "chat"), closed: "request_lost" }],
    notifications: [{ notification_id: "n2", kind: "question", text: "?", conversation_id: "chat" }],
    last_message: { text: "hi", role: "assistant", created_at_ms: 5 }, busy: "replying",
  })
  const questions = mergeQuestions(parseQuestions({ questions: [question("old", "chat"), question("other", "other")] }), pushed)
  assert.deepEqual(questions.map((item) => item.question_id), ["other", "open"], "answered and closed ones leave the list")
  assert.deepEqual(mergeQuestions(questions, { ...pushed, conversation_id: "other", questions: [] }).map((item) => item.question_id), ["open"])
  const notifications = mergeNotifications([{ notification_id: "n1", kind: "question", text: "", conversation_id: "chat" }, { notification_id: "x", kind: "question", text: "", conversation_id: "other" }], pushed)
  assert.deepEqual(notifications.map((item) => item.notification_id), ["x", "n2"])
  const bots = mergeBots([{ bot_id: "b", conversation_id: "chat", name: "", instructions: "", models: {}, skills: null, avatar: null, avatar_version: null, event_hash: "", last_message: null, busy: null }], pushed)
  assert.deepEqual([bots[0].last_message?.text, bots[0].busy], ["hi", "replying"])
  const idle = mergeBots(bots, records({ conversation_id: "chat" }))
  assert.deepEqual([idle[0].last_message?.text, idle[0].busy], ["hi", null], "a push without a message keeps the preview and still updates busy")
})

test("a work waits for confirmation only once its device cannot report it", () => {
  const node = (online: boolean, codex_connected?: boolean): NodeInfo[] => [{ device_id: "worker", name: "", local: false, online, revoked: false, capabilities: { manager: null, manager_state: null, relay: false, roots: [], codex_connected } }]
  const running = records({ works: [work("w1")] }).works[0]
  assert.equal(confirmReason(running, []), null, "an unknown device is not judged")
  assert.equal(confirmReason(running, node(true, true)), null)
  assert.equal(confirmReason(running, node(false)), "offline")
  assert.equal(confirmReason(running, node(true, false)), "disconnected")
  assert.equal(confirmReason({ ...running, kind: "task" }, node(true, false)), null, "a task does not need the App Server")
  assert.equal(confirmReason({ ...running, state: "unverified", outcome: "unverified" }, node(true, true)), "unverified")
  const open = mergeOpenWorks([], records({ open_works: [{ ...work("w1"), conversation_id: "chat" }] }))
  assert.deepEqual(workChecks(open, node(false)).map((item) => item.kind === "work" && [item.check_id, item.conversation_id, item.reason]), [["work:w1", "chat", "offline"]])
})

test("a push replaces its conversation's open works with the Node's list, whatever order works arrived in", () => {
  const open = (id: string, conversation_id: string, extra: Record<string, unknown> = {}) => ({ ...work(id, extra), conversation_id })
  const failed = { state: "finished", outcome: "failed" }
  let list = mergeOpenWorks([], records({ open_works: [open("w2", "chat", failed)] }))
  list = mergeOpenWorks(list, records({ conversation_id: "other", open_works: [open("x1", "other")] }))
  // An earlier work's result synced late: the Node still lists the later failure, and the page keeps it.
  list = mergeOpenWorks(list, records({ works: [work("w1", { state: "finished", outcome: "completed" })], open_works: [open("w2", "chat", failed)] }))
  assert.deepEqual(list.map((item) => item.work_id), ["x1", "w2"])
  list = mergeOpenWorks(list, records({ open_works: [open("w9", "chat", { state: "dispatch_rejected" })] }))
  assert.deepEqual(list.map((item) => [item.work_id, item.state]), [["x1", "running"], ["w9", "dispatch_rejected"]], "a refused dispatch is listed like a failure")
  list = mergeOpenWorks(list, records({ open_works: [] }))
  assert.deepEqual(list.map((item) => item.work_id), ["x1"], "a closed or answered failure leaves")
})

test("works one offline device holds wait on one card; an unverified one keeps its own", () => {
  const nodes: NodeInfo[] = [
    { device_id: "worker", name: "", local: false, online: false, revoked: false, capabilities: { manager: null, manager_state: null, relay: false, roots: [] } },
    { device_id: "mac", name: "", local: false, online: true, revoked: false, capabilities: { manager: null, manager_state: null, relay: false, roots: [], codex_connected: false } },
  ]
  const open = mergeOpenWorks([], records({ open_works: [
    work("w1"), work("w2", { state: "unverified", outcome: "unverified" }), work("w3", { target_node: "mac" }), work("w4"), work("w5", { target_node: "mac" }),
  ].map((item) => ({ ...item, conversation_id: "chat" })) }))
  const checks = workChecks(open, nodes)
  assert.deepEqual(checks.map((item) => [item.check_id, item.kind === "device" ? item.works.map((one) => one.work_id) : item.kind]), [
    ["device:worker:offline", ["w1", "w4"]], ["work:w2", "work"], ["device:mac:disconnected", ["w3", "w5"]],
  ])
})

test("a change the Node cannot map reloads only its conversation", () => {
  assert.deepEqual(parseChanged({ type: "changed", conversation_id: "chat" }), { type: "invalidate", conversation_id: "chat" })
  assert.deepEqual(parseChanged(undefined), { type: "invalidate" })
})
