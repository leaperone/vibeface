import assert from "node:assert/strict"
import test from "node:test"
import { openWorksFromOverview, overviewFromWorkspace, parseConversation, parseOpenWorks, parseWorkspaceOverview } from "./model.ts"
import { cardState, overviewScope, taskGroup, taskLabel, unrepresentedOverviewChecks } from "./labels.ts"
import type { BotRecord, NodeInfo, OpenWork, Question } from "./types.ts"

const bot = (conversation_id: string, bot_id = `bot-${conversation_id}`, name = conversation_id): BotRecord => ({
  bot_id, conversation_id, name, instructions: "", models: {}, skills: null, avatar: null, avatar_version: null,
  event_hash: "event", last_message: null, busy: null,
})
const node = (device_id: string, online = true, local = false): NodeInfo => ({
  device_id, name: device_id, local, online, revoked: false,
  capabilities: { manager: "manager", manager_state: "ready", relay: false, roots: [], codex_connected: true },
})
const work = (extra: Partial<OpenWork> = {}): OpenWork => ({
  work_id: "work-1", conversation_id: "chat-1", trigger: "message:m1", dispatcher: "gateway", call_ref: "dispatch-1",
  target_node: "worker", cwd: "/project", title: "检查部署", prompt: "检查", state: "running", phase: "running",
  summary: "执行中", outcome: null, result_text: "", failure_reason: null, kind: "codex", thread: "provider-thread",
  thread_id: "provider-thread", turn_id: "turn-1", parent: "parent-work", continues: "continued-work", ...extra,
})
const question = (extra: Partial<Question> = {}): Question => ({
  question_id: "question-1", conversation_id: "chat-1", bot: { bot_id: "bot-chat-1", name: "Bot" }, severity: "attention",
  text: "要继续吗？", choices: [{ label: "继续" }], about: { node: "worker", agent: "codex", session_id: "provider-thread", cwd: "/project", summary: "", request_id: "request-1" },
  author: "worker", created_at_ms: 1, answer: null, closed: null, ...extra,
})

test("the conversation parser keeps WorkProjection relation and phase fields", () => {
  const parsed = parseConversation({
    conversation: { conversation_id: "chat-1", title: "Bot", owner: "gateway", owner_epoch: 1 }, messages: [],
    works: [work()], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], questions: [], sessions: [], sent: [],
    bot_settings: { skills: [], schedules: [], memory: [] },
  }, "chat-1")
  assert.deepEqual(parsed.works[0] && {
    dispatcher: parsed.works[0].dispatcher, call_ref: parsed.works[0].call_ref, parent: parsed.works[0].parent,
    continues: parsed.works[0].continues, thread: parsed.works[0].thread, phase: parsed.works[0].phase,
  }, { dispatcher: "gateway", call_ref: "dispatch-1", parent: "parent-work", continues: "continued-work", thread: "provider-thread", phase: "running" })
  const old = parseOpenWorks({ works: [{ ...work(), dispatcher: undefined, call_ref: undefined, parent: undefined, continues: undefined, thread: undefined, phase: undefined }] })[0]
  assert.equal(old?.parent, undefined, "older Nodes do not invent relations")
})

test("the overview separates work, check, question and dispatch rows", () => {
  const unknown = work({ work_id: "unknown", state: "unverified", outcome: "unverified", thread_id: "different-thread" })
  const delivery = work({ work_id: "delivery", state: "dispatch_waiting", outcome: null, parent: undefined, thread: undefined, thread_id: undefined })
  const overview = overviewFromWorkspace({
    openWorks: [work(), unknown, delivery], bots: [bot("chat-1")], questions: [question()], nodes: [node("worker")],
  })
  assert.deepEqual(overview.tasks.map((item) => item.kind), ["work", "work", "dispatch", "check"])
  assert.equal(overview.questions[0]?.kind, "question")
  assert.equal(overview.questions[0]?.work_id, "work-1", "an approval may link to its thread work without becoming a parent edge")
  assert.equal(overview.tasks[0]?.parent, "parent-work")
  assert.equal(overview.tasks[0]?.thread_id, "provider-thread")
  assert.notEqual(overview.tasks[0]?.parent, overview.tasks[0]?.thread_id)
  assert.equal(overview.coverage.complete, false)
  assert.deepEqual(overview.coverage.remote_owners, [{ owner: "worker", reachable: true, reason: null }])
  assert.equal(overview.jobs.length, 0, "the local open_works snapshot is not a scheduler queue")
})

test("missing bots stay visible but cannot navigate, and unknown results stay unverified", () => {
  const overview = overviewFromWorkspace({
    openWorks: [work({ conversation_id: "deleted-chat", state: "unverified", outcome: "completed" })], bots: [], questions: [], nodes: [node("worker", false)],
  })
  const [task, check] = overview.tasks
  assert.equal(task?.bot, null)
  assert.equal(task?.navigation, null)
  assert.equal(task?.state, "unverified")
  assert.equal(task?.outcome, "unverified")
  assert.equal(check?.kind, "check")
  assert.equal(check?.state, "unknown")
  assert.equal(check?.outcome, "unverified")
  assert.deepEqual(overview.coverage.unreachable_nodes, ["worker"])
})

test("a bounded overview parser retains unknown and unreachable states", () => {
  const overview = parseWorkspaceOverview({
    schema: 1, generated_at_ms: 10, network_epoch: 2, consistency: "mixed_remote", truncated: false,
    bots: [], questions: [{ question_id: "answered", conversation_id: "chat", text: "已处理", severity: "info", answer: { text: "好" }, created_at_ms: 1 }], jobs: [{ conversation_id: "chat", trigger: "message:m1", state: "running", source_node: "worker" }],
    tasks: [{ work_id: "work-1", conversation_id: "chat", kind: "work", state: "running", outcome: null, target_node: "worker", cwd: "/project", remote_reachable: false },
      { work_id: "delivery-1", conversation_id: "chat", kind: "task", state: "accepted_waiting_sync", source: "local_dispatch", target_node: "worker", cwd: "/project", outcome: null }],
    deliveries: [{ work_id: "work-1", conversation_id: "chat", state: "stop_waiting", target_node: "worker", cwd: "/project" }],
    checks: [{ check_id: "check-1", conversation_id: "chat", trigger: "work:1", created_at_ms: 2 }],
    relations: [{ from_work: "child", to_work: "parent", kind: "parent" }],
    coverage: { bounded: true, local_heads: true, remote_owners: [{ owner: "worker", reachable: false, reason: "offline" }] },
  })
  assert.equal(overview.tasks[0]?.state, "stop_waiting")
  assert.equal(overview.tasks[0]?.outcome, null)
  assert.equal(overview.tasks[0]?.cwd, "/project")
  assert.equal(overview.tasks[1]?.kind, "dispatch")
  assert.equal(overview.tasks[1]?.cwd, "/project")
  assert.equal(overview.tasks[2]?.kind, "check")
  assert.equal(overview.tasks.filter((task) => task.work_id === "work-1").length, 1)
  assert.equal(overview.questions.length, 0)
  assert.equal(overview.jobs[0]?.state, "running")
  assert.equal(overview.coverage.remote_owners[0]?.reachable, false)
  assert.equal(overview.coverage.source, "workspace_overview")
  assert.deepEqual(overview.relations, [{ from_work: "child", to_work: "parent", kind: "parent" }])
})

test("an overview row the Node names only by its conversation shows that conversation's bot and opens its details", () => {
  const overview = parseWorkspaceOverview({
    bots: [{ bot_id: "bot-1", conversation_id: "chat", name: "助手" }],
    tasks: [{ work_id: "work-1", conversation_id: "chat", kind: "task", state: "finished", outcome: "failed", target_node: "worker", failure_reason: "Codex management timed out" }],
    deliveries: [{ work_id: "work-2", conversation_id: "chat", state: "dispatch_rejected", target_node: "worker", reason: "busy" }],
  })
  assert.deepEqual(overview.tasks.map((task) => task.bot?.name), ["助手", "助手"])
  assert.deepEqual(overview.tasks[0]?.navigation, { bot_id: "bot-1", conversation_id: "chat", work_id: "work-1" })
})

test("a failed open work stays on the task page", () => {
  const overview = overviewFromWorkspace({ openWorks: [work({ state: "finished", outcome: "failed", failure_reason: "boom" })], bots: [bot("chat-1")], questions: [], nodes: [node("worker")] })
  assert.deepEqual(overview.tasks.map((task) => [task.work_id, task.outcome]), [["work-1", "failed"]])
})

test("a bounded overview adds observations without deleting known work or replacing its full fields", () => {
  const known = Array.from({ length: 257 }, (_, index) => work({ work_id: `w-${index}`, prompt: `完整请求 ${index}`, result_text: "已有输出", trigger: "message:known" }))
  const overview = parseWorkspaceOverview({
    tasks: known.slice(0, 256).map((item) => ({ ...item, kind: "work", prompt: undefined, result_text: undefined, summary: "另一次读取" })),
    deliveries: [{ work_id: "delivery-only", conversation_id: "chat-1", state: "accepted_waiting_sync", target_node: "worker" },
      { work_id: "w-0", conversation_id: "other-chat", state: "stop_waiting", target_node: "other" }],
    truncated: true, consistency: "partial", coverage: { source: "workspace_overview", complete: false, truncated: true, remote_owners: [{ owner: "remote", reason: "remote_scheduler_not_queried" }] },
  })
  const merged = openWorksFromOverview(overview, known)
  assert.equal(merged.length, 259)
  assert.equal(merged[0], known[0], "the bounded read cannot establish that its fields are newer")
  assert.deepEqual(merged.slice(0, 257), known, "all prompt, result and causal fields remain intact")
  assert.equal(merged[257]?.state, "dispatch_waiting")
  assert.equal(merged[258]?.stop, "stop_waiting")
  assert.equal(cardState(merged[257], [], overview.tasks[256]), "accepted_waiting_sync")
  assert.equal(cardState(merged[258], [], overview.tasks[257]), "stop_waiting")
  const notes = overviewScope(overview.coverage).join(" · ")
  assert.match(notes, /部分记录/)
  assert.match(notes, /截断/)
  assert.match(notes, /尚未读取其他设备的后台工作/)
  assert.doesNotMatch(notes, /0 项|另有/, "unknown omission counts are not fabricated")
  assert.match(overviewScope({ ...overview.coverage, omitted_tasks: 3 }).join(" · "), /本次总览未返回 3 项任务/)
})

test("task confirmation outranks a running projection while delivery, stop and real outcomes remain distinct", () => {
  const running = work()
  const offline = [node("worker", false)]
  assert.equal(cardState(running, offline, { state: "running", outcome: null }), "awaiting_confirmation")
  assert.equal(taskGroup(cardState(running, offline, { state: "running", outcome: null }), false), "attention")
  const disconnected = [{ ...node("worker"), capabilities: { ...node("worker").capabilities!, codex_connected: false } }]
  assert.equal(cardState(running, disconnected, { state: "running", outcome: null }), "awaiting_confirmation")
  assert.equal(cardState(work({ state: "unverified", outcome: "unverified" }), [], { state: "running", outcome: null }), "awaiting_confirmation")
  assert.equal(cardState(work({ outcome: "unverified" }), [], { state: "running", outcome: null }), "unverified")
  for (const state of ["unknown", "unverified", "awaiting_confirmation"]) assert.equal(taskGroup(state, false), "attention")
  for (const state of ["dispatch_waiting", "accepted_waiting_sync", "stop_waiting", "stop_timed_out", "dispatch_unverified"] as const) assert.equal(cardState(running, [], { state, outcome: null }), state)
  for (const outcome of ["completed", "interrupted", "failed", "rejected"] as const) {
    const ended = work({ state: "finished", outcome })
    assert.equal(cardState(ended, offline, { state: "running", outcome: null }), outcome)
  }
  const ended = parseWorkspaceOverview({ tasks: [{ ...running, kind: "work", state: "finished", outcome: "completed", remote_reachable: false }] })
  assert.equal(ended.tasks[0]?.outcome, "completed", "offline now does not invalidate an already recorded result")
  const stopped = parseWorkspaceOverview({
    bots: [bot("chat-1")], tasks: [{ ...running, kind: "work", state: "finished", outcome: "completed" }, { ...running, work_id: "unknown", kind: "work", state: "unverified", outcome: "unverified" }],
    deliveries: [{ work_id: running.work_id, conversation_id: running.conversation_id, state: "stop_waiting" }, { work_id: "unknown", conversation_id: running.conversation_id, state: "stop_waiting" }],
    checks: [{ check_id: "process", conversation_id: "chat-1" }],
  })
  assert.equal(stopped.tasks[0]?.outcome, "completed", "a pending stop does not erase a terminal result")
  assert.equal(stopped.tasks[1]?.outcome, "unverified", "a stop does not replace an unknown result with an active state")
  assert.deepEqual(stopped.tasks[2]?.navigation, { bot_id: "bot-chat-1", conversation_id: "chat-1" })
})

test("unordered work snapshots retain terminal outcomes and leave ambiguous results unknown", () => {
  const observed = (extra: Partial<OpenWork>) => parseWorkspaceOverview({ tasks: [{ ...work(extra), kind: "work" }] }).tasks[0]
  const online = [node("worker")]
  const running = work()
  const failed = observed({ state: "finished", outcome: "failed", failure_reason: "总览记录的失败原因" })
  const state = cardState(running, online, failed)
  assert.equal(state, "failed", "a failed observation cannot become a generic finished success")
  assert.equal(taskLabel(state), "失败")
  assert.equal(taskGroup(state, false), "attention")
  assert.equal(cardState(work({ state: "finished", outcome: "failed" }), online, observed({})), "failed")
  assert.equal(cardState(running, online, observed({ state: "finished", outcome: "completed" })), "completed")
  assert.equal(cardState(work({ state: "finished", outcome: "completed" }), online, observed({})), "completed")
  assert.equal(cardState(work({ state: "unverified", outcome: "unverified" }), online, failed), "awaiting_confirmation")
  assert.equal(cardState(work({ state: "finished", outcome: "completed" }), online, observed({ state: "unverified", outcome: "unverified" })), "unverified")
  for (const [known, projected] of [["completed", "failed"], ["failed", "completed"]] as const) {
    assert.equal(cardState(work({ state: "finished", outcome: known }), online, observed({ state: "finished", outcome: projected })), "unknown", "neither snapshot exposes the finish rank that settles conflicting results")
  }
  assert.equal(cardState(running, online, observed({ state: "finished", outcome: null })), "unknown")
  assert.equal(cardState(work({ state: "dispatch_unverified" }), online, observed({ state: "finished", outcome: "completed" })), "dispatch_unverified")
  assert.equal(cardState(work({ state: "finished", outcome: "rejected" }), online, observed({})), "rejected", "retrying an existing DAG work ID returns its original result")
  const merged = openWorksFromOverview(parseWorkspaceOverview({ tasks: [{ ...work(), kind: "work", state: "finished", outcome: "failed" }] }), [running])
  assert.equal(merged[0], running, "display reconciliation never rewrites complete request, output or causal fields")
})

test("overview task checks only fold into a visible unverified work on this conversation and device", () => {
  const known = work({ kind: "task", state: "unverified", outcome: "unverified", target_node: "entry" })
  const nodes = [node("entry", true, true), node("worker")]
  const overview = parseWorkspaceOverview({ checks: [{ check_id: "process-work", conversation_id: known.conversation_id, trigger: `task:${known.work_id}` }, { check_id: "process-chat", conversation_id: known.conversation_id, trigger: "message:one" }] })
  assert.equal(overview.tasks[0]?.work_id, known.work_id)
  assert.deepEqual(overview.tasks[0]?.kind === "check" && overview.tasks[0].work_ids, [known.work_id])
  const ids = (works: OpenWork[], available = nodes) => unrepresentedOverviewChecks(overview, works, available).map((check) => check.check_id)
  assert.deepEqual(ids([known]), ["process-chat"])
  assert.deepEqual(ids([]), ["process-work", "process-chat"], "a bounded-out work cannot hide the only available check")
  assert.deepEqual(ids([{ ...known, conversation_id: "other-chat" }]), ["process-work", "process-chat"])
  assert.deepEqual(ids([{ ...known, target_node: "worker" }]), ["process-work", "process-chat"])
  assert.deepEqual(ids([known], []), ["process-work", "process-chat"], "the check device must be known, not guessed")
  assert.deepEqual(ids([{ ...known, state: "running", outcome: null }]), ["process-work", "process-chat"], "an active task does not replace a separate process check")
  const bounded = parseWorkspaceOverview({ tasks: [{ ...known, kind: "task" }], checks: [{ check_id: "process-work", conversation_id: known.conversation_id, trigger: `task:${known.work_id}` }] })
  assert.deepEqual(unrepresentedOverviewChecks(bounded, openWorksFromOverview(bounded, []), nodes), [], "an overview-only unverified work also represents its local task check")
})

test("bounded check identities use the complete known Bot catalog without changing the received snapshot", () => {
  const bots = Array.from({ length: 257 }, (_, index) => bot(`chat-${index + 1}`))
  const overview = parseWorkspaceOverview({ bots: bots.slice(0, 256), checks: [{ check_id: "only-check", conversation_id: "chat-257", trigger: "message:one" }], truncated: true })
  const checks = unrepresentedOverviewChecks(overview, [], [], bots)
  assert.deepEqual(checks[0]?.bot, bots[256])
  assert.deepEqual(checks[0]?.navigation, { bot_id: "bot-chat-257", conversation_id: "chat-257" })
  assert.equal(overview.tasks[0]?.bot, null, "the bounded snapshot remains the actual received observation")
  assert.equal(unrepresentedOverviewChecks(overview, [], [], bots.slice(0, 256))[0]?.bot, null)
  assert.equal(unrepresentedOverviewChecks(overview, [], [], [bot("other-chat", "bot-chat-257")])[0]?.navigation, null, "identity is matched by conversation, never by a coincident Bot id")
})
