import assert from "node:assert/strict"
import test from "node:test"
import { parseChanged } from "../lib/progress.ts"
import { botReadiness, confirmReason, parseConversation, parseNodes, parseQuestions, parseRecords, workChecks } from "./model.ts"
import { mergeBots, mergeConversation, mergeNotifications, mergeOpenWorks, mergeQuestions } from "./records.ts"
import type { NodeInfo } from "./types.ts"

const rank = (lamport: number) => `${String(lamport).padStart(20, "0")}:${"0".repeat(20)}:h${lamport}`
const message = (message_id: string, lamport: number) => ({ message_id, role: "user", text: message_id, event_hash: `e-${message_id}`, created_at_ms: lamport, rank: rank(lamport) })
const work = (work_id: string, extra: Record<string, unknown> = {}) => ({
  work_id, trigger: "t", target_node: "worker", cwd: "/w", title: "", prompt: "p", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, kind: "codex", ...extra,
})
const question = (question_id: string, conversation_id: string, answer: unknown = null) => ({
  question_id, conversation_id, bot: null, severity: "info", text: "?", choices: [],
  about: { node: "n", agent: "codex", session_id: "s", cwd: "/", summary: "" }, author: "n", created_at_ms: 1, answer,
})
const shown = parseConversation({
  conversation: { conversation_id: "chat", title: "", owner: "gateway", owner_epoch: 1 },
  messages: [message("m1", 1), message("m3", 3)], works: [work("w1"), work("w2")],
  deliveries: [{ state: "dispatch_waiting", work_id: "w9", target_node: "worker", cwd: "/w" }, { state: "stop_waiting", work_id: "w1", target_node: "worker" }],
  scheduler: { jobs: [{ trigger: "a", state: "queued" }], unanswered: ["m1"] }, checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [],
  questions: [], sessions: [],
}, "chat")
const records = (extra: Record<string, unknown> = {}) => {
  const event = parseChanged({
    type: "records", conversation_id: "chat", messages: [], works: [], last_message: null, questions: [], notifications: [],
    deliveries: [], checks: [], jobs: [], unanswered: [], sent: [], open_works: [], ...extra,
  })
  assert.equal(event.type, "records")
  return parseRecords(event.type === "records" ? event.value : null)
}

test("a pushed message lands in rank order and the push replaces deliveries, checks, jobs and questions", () => {
  const next = mergeConversation(shown, records({
    messages: [message("m2", 2)], works: [work("w2", { state: "finished", outcome: "completed" })],
    jobs: [{ trigger: "a", state: "completed" }], questions: [question("q", "chat")],
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
    messages: [], works: [], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], questions: [], sessions: [],
    scheduler: { jobs: [{ trigger: "message:m1", state: "running", source_node: "owner" }], unanswered: [], remote: { owner: "owner", reachable: true } },
  }, "chat")
  assert.deepEqual(away.scheduler?.remote, { owner: "owner", reachable: true })
  const next = mergeConversation(away, records({ jobs: [{ trigger: "message:m1", state: "pending_delivery" }, { trigger: "message:m2", state: "pending_delivery" }] }))
  assert.deepEqual(next.scheduler?.jobs.map((item) => [item.trigger, item.state]), [["message:m2", "pending_delivery"], ["message:m1", "running"]])
  const local = mergeConversation(shown, records({ jobs: [{ trigger: 'a', state: 'running', source_node: 'gateway' }] }))
  const finished = mergeConversation(local, records({ jobs: [{ trigger: 'a', state: 'completed', source_node: 'gateway' }] }))
  assert.deepEqual(finished.scheduler?.jobs.map((item) => [item.trigger, item.state]), [['a', 'completed']], 'local observations replace earlier local jobs')
})

test("context state and queue reasons survive records without replacing a remote owner's context", () => {
  const context = {
    attempt_id: 'attempt', provider: 'codex', stage: 'compiled', session_generation: 2,
    stable_fingerprint: 'a'.repeat(64), dynamic_fingerprint: 'b'.repeat(64),
    stable_bytes: 100, dynamic_bytes: 200, invalidation_reason: 'stable_prompt_or_provider_changed',
    coverage: { messages_omitted: 3, works_omitted: 0, dispatches_omitted: 1, questions_omitted: 0, attachments_omitted: 0, memory_omitted: 2, checkpoint_omitted: 1, handoff_omitted: 0, pending_checks_omitted: 2, bot_indexes_omitted: { skills: 3 }, history_retrieval: 'context' },
    session_status: { resumable: true, pending_generation: true, native_id: 'private-native-id' },
    native_token: 'must-not-enter-client-state', rendered: 'private context',
  }
  const conflict = { call_ref: 'remember', memory_id: 'memory', expected_epoch: 9, actual_epoch: 9, expected_revision: '', actual_revision: 'new', reason: 'revision_changed' }
  const memory = { memory_id: 'memory', text: '已保存的事实', metadata: { epoch: 11, revision: 'c'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: false, source_event_hash: 'd'.repeat(64), native_token: 'private-memory-token' } }
  const sync = { items: [{ memory_id: 'memory', losing_revision: 'a'.repeat(64), winning_revision: 'c'.repeat(64), losing_event_hash: 'b'.repeat(64), winning_event_hash: 'd'.repeat(64), reason: 'user_protected', text: 'private-conflict-text', signature: 'private-conflict-signature' }], has_more: true }
  const update = records({ context, memory: [memory], memory_staged_count: 2, memory_conflicts: [conflict], memory_sync_conflicts: sync, jobs: [{ trigger: 'a', state: 'queued', lane: 'main', hold_reason: 'active_turn', pending_check: false, context }] })
  const next = mergeConversation(shown, update)
  assert.equal(next.scheduler?.context?.stage, 'compiled')
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
  assert.deepEqual(next.bot_settings.memory[0].metadata, { epoch: 11, revision: 'c'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: false, source_event_hash: 'd'.repeat(64) })
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
  assert.throws(() => records({ context: { ...context, stable_fingerprint: 'invalid' } }))
  assert.throws(() => records({ context: { ...context, session_status: { resumable: 'yes', pending_generation: false } } }))
  assert.throws(() => records({ context: { ...context, coverage: { ...context.coverage, bot_indexes_omitted: { skills: -1 } } } }))
  assert.throws(() => records({ memory_staged_count: -1 }))
  assert.throws(() => records({ memory: [{ ...memory, metadata: { ...memory.metadata, origin: 'unknown' } }] }))
  assert.equal(mergeConversation(next, records({ memory_staged_count: null, memory_conflicts: null })).bot_settings.memory_staged_count, null)
  for (const hold_reason of ['provider_serial', 'task_limit', 'machine_slot', 'pending_check', 'backend_config_pending']) assert.equal(records({ jobs: [{ trigger: 'a', state: 'queued', hold_reason }] }).jobs[0].hold_reason, hold_reason)
})

test('a task coordination queue stays separate from its shared work state', () => {
  const [task] = records({ works: [work('task', { kind: 'task', state: 'running', coordination_job: { trigger: 'task:task', state: 'queued', lane: 'task', hold_reason: 'machine_slot', native_token: 'private-task-token' } })] }).works
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
