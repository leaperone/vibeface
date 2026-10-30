import assert from "node:assert/strict"
import test from "node:test"
import { overviewFromWorkspace, parseConversation, parseOpenWorks, parseWorkspaceOverview } from "./model.ts"
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
    tasks: [{ work_id: "work-1", conversation_id: "chat", kind: "work", state: "finished", outcome: "completed", target_node: "worker", remote_reachable: false },
      { work_id: "delivery-1", conversation_id: "chat", kind: "task", state: "accepted_waiting_sync", source: "local_dispatch", target_node: "worker", outcome: null }],
    deliveries: [{ work_id: "stop-1", conversation_id: "chat", state: "stop_waiting", target_node: "worker" }],
    checks: [{ check_id: "check-1", conversation_id: "chat", trigger: "work:1", created_at_ms: 2 }],
    relations: [{ from_work: "child", to_work: "parent", kind: "parent" }],
    coverage: { bounded: true, local_heads: true, remote_owners: [{ owner: "worker", reachable: false, reason: "offline" }] },
  })
  assert.equal(overview.tasks[0]?.state, "unverified")
  assert.equal(overview.tasks[0]?.outcome, "unverified")
  assert.equal(overview.tasks[1]?.kind, "dispatch")
  assert.equal(overview.tasks[2]?.kind, "dispatch")
  assert.equal(overview.tasks[3]?.kind, "check")
  assert.equal(overview.questions.length, 0)
  assert.equal(overview.jobs[0]?.state, "running")
  assert.equal(overview.coverage.remote_owners[0]?.reachable, false)
  assert.equal(overview.coverage.source, "workspace_overview")
  assert.deepEqual(overview.relations, [{ from_work: "child", to_work: "parent", kind: "parent" }])
})
