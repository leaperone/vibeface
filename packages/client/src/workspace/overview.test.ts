import assert from "node:assert/strict"
import test from "node:test"
import { openWorksFromOverview, overviewFromWorkspace, parseConversation, parseOpenWorks, parseWorkspaceOverview } from "./model.ts"
import { cardState, overviewScope, taskGroup, taskLabel, unrepresentedOverviewChecks, workspaceActivities } from "./labels.ts"
import type { BotRecord, NodeInfo, OpenWork, PendingCheck, Question } from "./types.ts"

const bot = (conversation_id: string, bot_id = `bot-${conversation_id}`, name = conversation_id): BotRecord => ({
  bot_id, conversation_id, name, instructions: "", models: {}, skills: null, avatar: null, avatar_version: null,
  event_hash: "event", last_message: null, busy: null,
})
const node = (device_id: string, online = true, local = false): NodeInfo => ({
  device_id, name: device_id, local, online, revoked: false,
  capabilities: { manager: "manager", manager_state: "ready", relay: false, roots: [], codex_connected: true },
})
const work = (extra: Record<string, unknown> = {}): OpenWork => parseOpenWorks({ works: [{
  work_id: "work-1", conversation_id: "chat-1", trigger: "message:m1", dispatcher: "gateway", call_ref: "dispatch-1",
  target_node: "worker", cwd: "/project", title: "检查部署", prompt: "检查", state: "running", phase: "running",
  summary: "执行中", outcome: null, result_text: "", failure_reason: null, kind: "codex", thread: "provider-thread",
  thread_id: "provider-thread", turn_id: "turn-1", parent: "parent-work", continues: "continued-work", resolved: { provider: "codex", model: null }, ...extra,
}] })[0]!
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

test("accepted records require a frozen selection while dispatch and overview cards stay summaries", () => {
  const accepted = work()
  assert.deepEqual(accepted.resolved, { provider: "codex", model: null })
  const { resolved: _selection, ...missing } = accepted
  assert.throws(() => parseOpenWorks({ works: [missing] }))
  assert.throws(() => parseOpenWorks({ works: [{ ...missing, resolved: null }] }))
  assert.throws(() => parseOpenWorks({ works: [{ ...missing, resolved: { provider: "codex" } }] }))
  const [dispatch] = parseOpenWorks({ works: [{ ...missing, state: "dispatch_waiting" }] })
  assert.equal(dispatch?.state, "dispatch_waiting")
  assert.equal(dispatch?.resolved, undefined)
  const overview = overviewFromWorkspace({ openWorks: [accepted], bots: [bot("chat-1")], questions: [], nodes: [node("worker")] })
  const [card] = openWorksFromOverview(overview, [])
  assert.equal(card?.work_id, accepted.work_id)
  assert.equal(card?.state, "running")
  assert.equal(card && "resolved" in card, false)
  assert.equal(card && "prompt" in card, false)
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
  assert.deepEqual(ids([work({ ...known, state: "running", outcome: null })]), ["process-work", "process-chat"], "an active task does not replace a separate process check")
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

test("workspace activities attach approval facts, keep task checks with work, and leave other turns separate", () => {
  const running = work({ thread_id: "provider-thread" })
  const approval = question({ about: { node: "worker", agent: "codex", session_id: "provider-thread", cwd: "/project", summary: "", request_id: "request-1" } })
  const workCheck: PendingCheck = { kind: "work", check_id: "work:work-1", detail: "", work_id: running.work_id, target_node: running.target_node, conversation_id: running.conversation_id, title: running.title, reason: "offline" }
  const overview = parseWorkspaceOverview({
    tasks: [
      { ...running, kind: "work" },
    ],
    checks: [{ check_id: "process-work", conversation_id: running.conversation_id, trigger: `task:${running.work_id}` }],
    questions: [{ question_id: approval.question_id, conversation_id: approval.conversation_id, text: approval.text, severity: approval.severity, choices: approval.choices, created_at_ms: approval.created_at_ms, answer: null, closed: null, about: approval.about }],
    jobs: [{ job_id: "job-1", conversation_id: running.conversation_id, trigger: "message:ordinary", state: "queued", message_id: "message-1" }],
    truncated: true,
  })
  const view = workspaceActivities({ works: [running], questions: [approval], checks: [workCheck], overview, nodes: [node("worker")], read: true, connected: true, answered: {} })
  assert.equal(view.activities.length, 2, "approval, pending check and task process check share one work activity; ordinary job is a turn")
  const workActivity = view.activities.find((activity) => activity.kind === "work")
  assert.ok(workActivity)
  assert.equal(workActivity?.facts.questions.length, 1)
  assert.equal(workActivity?.facts.checks.length, 1)
  assert.equal(workActivity?.facts.overviewChecks.length, 1)
  assert.equal(workActivity?.group, "waiting")
  assert.equal(view.activities.filter((activity) => activity.kind === "turn").length, 1)
  const answered = workspaceActivities({ works: [running], questions: [approval], checks: [workCheck], overview, nodes: [node("worker")], read: true, connected: true, answered: { [approval.question_id]: "继续" } })
  assert.equal(answered.activities.length, 2, "answering removes only the attention fact and never drops the work")
  assert.ok(["working", "waiting"].includes(answered.activities.find((activity) => activity.kind === "work")?.group ?? ""), "the work remains running or waiting on its own unresolved device check")
  const parentChild = workspaceActivities({ works: [running, { ...running, work_id: "child", parent: running.work_id }], questions: [], checks: [], nodes: [node("worker")], read: true, connected: true })
  assert.equal(parentChild.activities.filter((activity) => activity.kind === "work").length, 2, "parent and child remain separate activities")
  const processOnly = workspaceActivities({ works: [running], questions: [], checks: [{ kind: "process", check_id: "process", detail: "still alive" }], nodes: [node("worker")], read: true, connected: true })
  assert.equal(processOnly.activities.filter((activity) => activity.kind === "attention").length, 1)
  const mismatched = workspaceActivities({ works: [running], questions: [], checks: [{ ...workCheck, conversation_id: "other-chat" }, { ...workCheck, target_node: "other" }], nodes: [node("worker"), node("other")], read: true, connected: true })
  assert.equal(mismatched.activities.filter((activity) => activity.kind === "attention").length, 2, "different conversations and devices do not dedupe")
  assert.equal(workspaceActivities({ works: [{ ...running, state: "unverified", outcome: "unverified" }], questions: [], checks: [], nodes: [node("worker")], read: true, connected: true }).groups.attention.length, 1)
})

test("workspace activities show a standalone question once across full and overview records", () => {
  const pending = question()
  const overview = overviewFromWorkspace({ openWorks: [], bots: [], questions: [pending], nodes: [] })
  const activities = workspaceActivities({ works: [], questions: [pending], checks: [], overview }).activities
  assert.equal(activities.length, 1)
  assert.equal(activities[0]?.kind === "attention" && activities[0].attention, pending, "keep the complete actionable question")
  assert.equal(workspaceActivities({ works: [], questions: [], checks: [], overview }).activities.length, 1, "an overview-only question stays visible")
  const other = { ...overview, questions: overview.questions.map((entry) => ({ ...entry, conversation_id: "other-chat" })) }
  assert.equal(workspaceActivities({ works: [], questions: [pending], checks: [], overview: other }).activities.length, 2, "a different conversation is a separate question")
  assert.equal(workspaceActivities({ works: [], questions: [pending], checks: [], overview, answered: { [pending.question_id]: "继续" } }).activities.length, 0)
})

test("workspace activities merge same-source job observations without hiding newer or uncertain state", () => {
  const runtime = { provider: "codex" as const, model: "frozen", reasoning_effort: null, generation: "g", session_generation: 1, delivery_id: "d", stage: "run" as const, execution: "started" as const, attempted_at_ms: 2, switched_from: null, switch_reason: null }
  const overview = parseWorkspaceOverview({ jobs: [{ job_id: "job-1", conversation_id: "chat-1", trigger: "message:ordinary", state: "queued", source_node: "entry" }] })
  const local = { conversation_id: "chat-1", job: { trigger: "message:ordinary", state: "running", runtime, failure: { kind: "unknown" as const, detail: "保留本地说明" } } }
  const input = { works: [], questions: [], checks: [], overview, jobs: [local], nodes: [node("entry", true, true)] }
  const merged = workspaceActivities(input).activities
  assert.equal(merged.length, 1)
  assert.equal(merged[0]?.kind, "turn")
  if (merged[0]?.kind === "turn") {
    assert.equal(merged[0].state, "running")
    assert.equal(merged[0].job.runtime?.model, "frozen")
    assert.equal("failure" in merged[0].job ? merged[0].job.failure?.detail : undefined, "保留本地说明")
    assert.equal(merged[0].facts.jobs.length, 2, "both observations remain available to the activity")
  }
  const failed = workspaceActivities({ ...input, jobs: [{ ...local, job: { ...local.job, state: "failed", outcome: "unknown" as const } }] }).activities
  assert.equal(failed[0]?.kind === "turn" && failed[0].state, "failed", "a newer terminal observation is not hidden by queued overview data")
  const unknown = workspaceActivities({ ...input, jobs: [{ ...local, job: { ...local.job, state: "queued", pending_check: true } }] }).activities
  assert.equal(unknown[0]?.kind === "turn" && unknown[0].state, "unknown", "a pending check stays uncertain")
  for (const state of ["reconciling", "publishing"]) {
    const observed = workspaceActivities({ ...input, jobs: [{ ...local, job: { ...local.job, state } }] }).activities
    assert.equal(observed[0]?.kind === "turn" && observed[0].state, state, `${state} is not treated as an old queued observation`)
  }
  const steeredOverview = parseWorkspaceOverview({ jobs: [{ job_id: "job-3", conversation_id: "chat-1", trigger: "message:ordinary", state: "steered", source_node: "entry" }] })
  assert.equal(workspaceActivities({ ...input, overview: steeredOverview, jobs: [{ ...local, job: { ...local.job, state: "steered", outcome: "answered" as const } }] }).activities.length, 0, "steered history remains hidden after reconciliation")
  assert.equal(workspaceActivities({ ...input, jobs: [{ ...local, job: { ...local.job, state: "steered", outcome: "answered" as const } }] }).activities.length, 0, "steered history remains hidden without an overview duplicate")
  const overviewRunning = parseWorkspaceOverview({ jobs: [{ job_id: "job-2", conversation_id: "chat-1", trigger: "message:ordinary", state: "running", source_node: "entry" }] })
  const failedConflict = workspaceActivities({ ...input, overview: overviewRunning, jobs: [{ ...local, job: { ...local.job, state: "failed", retryable: true } }] }).activities
  assert.equal(failedConflict[0]?.kind === "turn" && failedConflict[0].state, "unknown", "an active and terminal observation has no ordering proof")
  if (failedConflict[0]?.kind === "turn") assert.equal(failedConflict[0].job.retryable, false, "an uncertain merge cannot become retryable")
  for (const [overviewState, localState] of [["claimed", "queued"], ["queued", "claimed"], ["steering", "queued"]] as const) {
    const rollback = parseWorkspaceOverview({ jobs: [{ job_id: `job-${overviewState}`, conversation_id: "chat-1", trigger: "message:ordinary", state: overviewState, source_node: "entry" }] })
    assert.equal(workspaceActivities({ ...input, overview: rollback, jobs: [{ ...local, job: { ...local.job, state: localState } }] }).activities[0]?.state, "unknown", `${overviewState} and ${localState} recovery reads have no ordering proof`)
  }
  const otherAttempt = parseWorkspaceOverview({ jobs: [{ job_id: "job-attempt", conversation_id: "chat-1", trigger: "message:ordinary", state: "running", source_node: "entry", runtime: { ...runtime, delivery_id: "different-attempt" } }] })
  const attemptConflict = workspaceActivities({ ...input, overview: otherAttempt }).activities
  assert.equal(attemptConflict[0]?.kind === "turn" && attemptConflict[0].state, "unknown", "different attempt identities remain uncertain")
  if (attemptConflict[0]?.kind === "turn") assert.equal(attemptConflict[0].facts.jobs.length, 2)
  const arbitrary = workspaceActivities({ ...input, jobs: [{ ...local, job: { ...local.job, state: "future_state" } }] }).activities
  assert.equal(arbitrary[0]?.kind === "turn" && arbitrary[0].state, "unknown", "an unrecognized scheduler state stays conservative")
  assert.equal(workspaceActivities({ ...input, overview: overviewRunning, jobs: [{ ...local, job: { ...local.job, state: "queued" } }] }).activities[0]?.state, "running", "the overview observation can be newer without preferring the local source")
  assert.equal(workspaceActivities({ ...input, nodes: [] }).activities.length, 2, "an unknown local identity cannot prove a duplicate")
  assert.equal(workspaceActivities({ ...input, jobs: [{ ...local, conversation_id: "other-chat" }] }).activities.length, 2)
  assert.equal(workspaceActivities({ ...input, jobs: [{ ...local, job: { ...local.job, source_node: "other-node" } }] }).activities.length, 2)
})

test("workspace activities dedupe live and overview questions, keep approvals on one work, and hide scheduler history", () => {
  const running = work()
  const sibling = work({ work_id: "work-2" })
  const approval = question()
  const overview = overviewFromWorkspace({ openWorks: [running, sibling], bots: [bot("chat-1")], questions: [approval], nodes: [node("worker")] })
  const view = workspaceActivities({
    works: [running, sibling], questions: [approval], checks: [], overview,
    jobs: [
      { conversation_id: "chat-1", job: { trigger: "message:done", state: "completed" } },
      { conversation_id: "chat-1", job: { trigger: "message:queue", state: "queued" } },
    ], nodes: [node("worker")], read: true, connected: true, answered: {},
  })
  const workActivities = view.activities.filter((activity) => activity.kind === "work")
  assert.equal(workActivities.find((activity) => activity.work_id === running.work_id)?.facts.questions.length, 1)
  assert.equal(workActivities.find((activity) => activity.work_id === sibling.work_id)?.facts.questions.length, 0)
  assert.equal(view.activities.filter((activity) => activity.kind === "attention").length, 0)
  assert.deepEqual(view.activities.filter((activity): activity is Extract<typeof view.activities[number], { kind: "turn" }> => activity.kind === "turn").map((activity) => activity.job.state), ["queued"])
})

test("overview-only questions remain one attention item instead of attaching to every sibling work", () => {
  const running = work()
  const sibling = work({ work_id: "work-2" })
  const overview = parseWorkspaceOverview({ questions: [{
    question_id: "overview-q", conversation_id: "chat-1", text: "继续吗？", severity: "attention", choices: [],
    about: { node: "worker", agent: "codex", session_id: "provider-thread", cwd: "/project", summary: "", request_id: "request" }, work_id: running.work_id, created_at_ms: 2,
  }] })
  const view = workspaceActivities({ works: [running, sibling], questions: [], checks: [], overview, nodes: [node("worker")], read: true, connected: true, answered: {} })
  assert.deepEqual(view.activities.filter((activity) => activity.kind === "attention").map((activity) => activity.id), ["attention:overview-question:overview-q"])
  assert.equal(view.activities.filter((activity) => activity.kind === "work").some((activity) => activity.facts.overviewQuestions.length > 0), false)
})

test("workspace activities dedupe overview and live scheduler jobs by trusted source", () => {
  const overview = parseWorkspaceOverview({ jobs: [{ job_id: "job-queue", conversation_id: "chat-1", trigger: "message:queue", state: "queued", source_node: "entry" }] })
  const view = workspaceActivities({
    works: [], questions: [], checks: [], overview,
    jobs: [{ conversation_id: "chat-1", job: { trigger: "message:queue", state: "running" } }],
    nodes: [node("entry", true, true)], read: true, connected: true,
  })
  const turns = view.activities.filter((activity): activity is Extract<typeof view.activities[number], { kind: "turn" }> => activity.kind === "turn")
  assert.deepEqual(turns.map((activity) => activity.id), ["turn:chat-1:local:message:queue"])
  assert.equal(turns[0]?.job.state, "running", "the live conversation snapshot wins over an older overview job")
})
