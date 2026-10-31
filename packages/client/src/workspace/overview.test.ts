import assert from "node:assert/strict"
import test from "node:test"
import { openWorksFromOverview, overviewFromWorkspace, parseConversation, parseOpenWorks, parseWorkspaceOverview } from "./model.ts"
import type { BotRecord, NodeInfo, OpenWork, Question } from "./types.ts"

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
  const [card] = openWorksFromOverview(overview)
  assert.equal(card?.work_id, accepted.work_id)
  assert.equal(card?.state, "running")
  assert.equal(card && "resolved" in card, false)
  assert.equal(card && "prompt" in card, false)
})
