import assert from "node:assert/strict"
import test from "node:test"
import { parseAnswer, parseConversation, parseOpenWorks, parsePrimary, parseQuestions, parseTriageSettings } from "./model.ts"
import { answerBody, answerLabel, awaitsAnswer, declined, foldAnswers, openQuestions, placeByTime, projectName, questionMeta, questionParts, settledAnswer, sharedProject, workApproval } from "./questions.ts"

const about = { node: "worker", agent: "codex", session_id: "s", cwd: "/path/to/web/", summary: "改登录" }
const row = (question_id: string, created_at_ms: number, extra: Record<string, unknown> = {}) => ({
  question_id, conversation_id: "chat", bot: { bot_id: "bot", name: "Bot" }, severity: "attention", text: "要不要继续",
  choices: [{ label: "继续" }, { label: "看看" }], about, author: "gateway", created_at_ms, answer: null, ...extra,
})
const snapshot = (extra: Record<string, unknown>) => ({
  conversation: { conversation_id: "chat", title: "Bot", owner: "gateway", owner_epoch: 1 }, messages: [], works: [], deliveries: [],
  checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [] }, sent: [], questions: [], sessions: [], ...extra,
})

test("questions parse from the list and the chat snapshot, and fail closed on shapes the web cannot show", () => {
  const [question, approval] = parseQuestions({ questions: [row("q1", 1), row("s1", 2, { about: { ...about, request_id: "r" } })] })
  assert.deepEqual(question.choices, [{ label: "继续" }, { label: "看看" }])
  const { choices: _, ...open } = row("q2", 3)
  assert.deepEqual(parseQuestions({ questions: [open] })[0].choices, [], "a question the node sent without choices is free text only")
  assert.equal(approval.about.request_id, "r")
  const answered = row("q2", 3, { answer: { message_id: "a-q2", text: "继续", author: "client", created_at_ms: 4 } })
  const chat = parseConversation(snapshot({
    questions: [answered],
    sessions: [{ node: "worker", agent: "codex", session_id: "s", cwd: "/w", summary: "改登录", muted: true, created_at_ms: 2 }],
    messages: [{ message_id: "a-q2", role: "user", text: "继续", author: "client", created_at_ms: 4, event_hash: "h", answers: "q2" }],
  }))
  assert.equal(chat.questions[0].answer?.text, "继续", "answered questions stay in the chat")
  assert.equal(chat.sessions[0].muted, true)
  assert.equal(chat.messages[0].answers, "q2")
  for (const bad of [{ severity: "loud" }, { about: null }, { answer: { text: "继续" } }, { choices: [{}] }, { conversation_id: 1 }, { conversation_id: null }, { closed: "gone" }])
    assert.throws(() => parseQuestions({ questions: [row("q", 1, bad)] }), Error, JSON.stringify(bad))
  assert.throws(() => parseConversation(snapshot({ sessions: [{ node: "worker" }] })))
})

test("the primary bot comes with the bot list, and none before any exists", () => {
  assert.equal(parsePrimary({ bots: [], primary: "chat" }), "chat")
  assert.equal(parsePrimary({ bots: [], primary: null }), null)
  assert.throws(() => parsePrimary({ bots: [], primary: 1 }))
})

test("an answer result is the question's own message, or it is not trusted", () => {
  assert.deepEqual(parseAnswer({ status: "answered", message: { message_id: "a-q1", text: "继续" } }, "q1"), { status: "answered", text: "继续" })
  assert.deepEqual(parseAnswer({ status: "already_answered", message: { message_id: "a-q1", text: "停下" } }, "q1"), { status: "already_answered", text: "停下" })
  assert.throws(() => parseAnswer({ status: "queued", message: { message_id: "a-q1", text: "继续" } }, "q1"), /无法识别/)
  assert.throws(() => parseAnswer({ status: "answered", message: { message_id: "a-q2", text: "继续" } }, "q1"), /对不上/)
})

test("an answer is signed as one message per question", () => {
  assert.deepEqual(answerBody("q1", "继续"), { type: "message_posted", message_id: "a-q1", role: "user", text: "继续", report_results: true, answers: "q1" })
})

test("待处理 lists exactly the open questions, urgent first, then newest first", () => {
  const questions = parseQuestions({ questions: [row("old", 1), row("done", 3, { answer: { message_id: "a-done", text: "继续", author: "c", created_at_ms: 4 } }), row("new", 2), row("urgent", 0, { severity: "urgent" }),
    row("lost", 5, { about: { ...about, request_id: "r" }, closed: "request_lost" })] })
  assert.equal(questions[4].closed, "request_lost")
  assert.deepEqual(openQuestions(questions).map((question) => question.question_id), ["urgent", "new", "old"])
})

test("an answer folds into its question instead of showing as a message", () => {
  const questions = parseQuestions({ questions: [row("q1", 1), row("q2", 2, { answer: { message_id: "a-q2", text: "停下", author: "c", created_at_ms: 3 } })] })
  const messages = [
    { message_id: "a-q1", text: "继续", answers: "q1" },
    { message_id: "a-q2", text: "停下", answers: "q2" },
    { message_id: "a-gone", text: "别处的回答", answers: "gone" },
    { message_id: "m", text: "普通消息" },
  ]
  const { answers, hidden } = foldAnswers(messages, questions)
  assert.deepEqual([...answers], [["q2", "停下"], ["q1", "继续"]])
  assert.deepEqual([...hidden], ["a-q1", "a-q2"], "an answer whose question is not on screen still shows")
})

test("questions and hand-offs sit where they happened in the chat", () => {
  const messages = [{ message_id: "m1", created_at_ms: 10 }, { message_id: "m2", created_at_ms: 20 }]
  const { before, rest } = placeByTime(messages, [{ id: "late", created_at_ms: 25 }, { id: "b", created_at_ms: 15 }, { id: "a", created_at_ms: 12 }, { id: "first", created_at_ms: 5 }])
  assert.deepEqual(before.get("m1")?.map((item) => item.id), ["first"])
  assert.deepEqual(before.get("m2")?.map((item) => item.id), ["a", "b"])
  assert.deepEqual(rest.map((item) => item.id), ["late"])
  assert.equal(projectName("/path/to/web/"), "web")
  assert.equal(projectName("C:\\code\\api"), "api")
  const nodes = [{ device_id: "mac", capabilities: { manager: null, manager_state: null, relay: false, roots: ["/Users/me/demo-project/", "/srv/api"] } }]
  assert.deepEqual(sharedProject("workspace-1/", "mac", nodes), { name: "demo-project", path: "/Users/me/demo-project/" }, "a device's own folder shows by its name")
  assert.deepEqual(sharedProject("workspace-2/web/src", "mac", nodes), { name: "src", path: "/srv/api/web/src" })
  assert.equal(sharedProject("workspace-3/x", "mac", nodes).name, "x", "a folder the device no longer lists keeps the shared path")
  assert.equal(sharedProject("workspace-1", "other", nodes).name, "workspace-1")
})

test("questions go to the web, or to the web and Grok; nothing else", () => {
  const status = { budget: { day: "2026-10-30", used: 100, limit: 100, retry_at_ms: 1793404800000, paused_reason: "budget_exhausted" }, pending_events: 2, inflight_events: 0, unknown_events: 1 }
  const settings = (default_outlet: string) => ({ default_outlet, projects: [], webhook_url: null, scan_codex_sessions: false, codex_sessions: null, webhook_secret_set: false, decision_url: "", device_id: "d", decision_tokens: [], outbox: { pending: 0, delivered: 0, dead: 0 }, status })
  assert.equal(parseTriageSettings(settings("both")).default_outlet, "both")
  assert.equal(parseTriageSettings(settings("record")).default_outlet, "record")
  for (const old of ["webhook", "off"]) assert.throws(() => parseTriageSettings(settings(old)), /无法识别/)
  assert.deepEqual(parseTriageSettings(settings("record")).status, status)
  assert.throws(() => parseTriageSettings({ ...settings("record"), status: { ...status, budget: { ...status.budget, paused_reason: "retry" } } }), /暂停原因无法识别/)
  assert.throws(() => parseTriageSettings({ ...settings("record"), status: { ...status, unknown_events: -1 } }), /数字不对/)
})

test("an answer replaced by another side's says so instead of vanishing", () => {
  assert.equal(settledAnswer(undefined, undefined), undefined, "an open question has nothing settled")
  assert.deepEqual(settledAnswer(undefined, "继续"), { text: "继续", elsewhere: false }, "this browser's answer shows before the network has it")
  assert.deepEqual(settledAnswer("继续", "继续"), { text: "继续", elsewhere: false })
  assert.deepEqual(settledAnswer("停下", undefined), { text: "停下", elsewhere: false }, "an answer given elsewhere with nothing tried here")
  assert.deepEqual(settledAnswer("停下", "继续"), { text: "停下", elsewhere: true }, "the other side's answer won the race")
})

test("only an open question waits on the person: a closed one is no count and no reason to hide 思考中", () => {
  const questions = parseQuestions({ questions: [row("open", 1), row("lost", 2, { about: { ...about, request_id: "r" }, closed: "request_lost" }),
    row("done", 3, { answer: { message_id: "a-done", text: "继续", author: "c", created_at_ms: 4 } }), row("here", 5), row("chat", 6)] })
  const waiting = questions.filter((question) => awaitsAnswer(question, { here: "继续" }, new Map([["chat", "停下"]])))
  assert.deepEqual(waiting.map((question) => question.question_id), ["open"])
})

test("an approval card names the task its request holds and leaves the device this screen goes through unnamed", () => {
  const works = parseOpenWorks({ works: [
    { work_id: "w-done", conversation_id: "chat", thread_id: "s", trigger: "t", target_node: "worker", cwd: "/w", title: "旧的", prompt: "p", state: "finished", summary: "", outcome: "completed", result_text: "", failure_reason: null, kind: "codex" },
    { work_id: "w1", conversation_id: "chat", thread_id: "s", trigger: "t", target_node: "worker", cwd: "/w", title: "", prompt: "p", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, kind: "codex" },
  ] })
  const bots = [{ bot_id: "bot", conversation_id: "chat" }]
  const [approval, question] = parseQuestions({ questions: [row("s1", 1, { about: { ...about, request_id: "r" } }), row("q1", 2)] })
  assert.deepEqual(questionMeta(approval, works, bots, "worker"), { approval: true, task: { bot_id: "bot", work_id: "w1", title: "任务" }, local: true })
  assert.deepEqual(questionMeta(approval, works, bots, "gateway"), { approval: true, task: { bot_id: "bot", work_id: "w1", title: "任务" }, local: false })
  assert.equal(questionMeta(approval, works, [], "worker").task, null, "a task whose bot is gone has nowhere to link")
  assert.deepEqual(questionMeta(question, works, bots, "gateway"), { approval: false, task: null, local: false }, "a plain question holds no task")
})

test("a refused request reads as refused, never as agreed", () => {
  for (const answer of ["拒绝", "不允许", "No", "deny", "拒绝，先别改"]) assert.ok(declined(answer), answer)
  for (const answer of ["允许", "同意", "nothing to add", "继续"]) assert.ok(!declined(answer), answer)
  assert.equal(answerLabel("拒绝"), "已拒绝")
  assert.equal(answerLabel("拒绝，先别改"), "已拒绝 · 拒绝，先别改")
  assert.equal(answerLabel("允许"), "已回答 · 允许")
})

test('a question asks on its first line and lists what it applies to after it', () => {
  assert.deepEqual(questionParts('要运行这个命令吗？\n\n命令：npm test\n在项目里'), { ask: '要运行这个命令吗？', lines: [{ label: '命令', value: 'npm test' }, { label: null, value: '在项目里' }] })
  assert.deepEqual(questionParts('继续吗？'), { ask: '继续吗？', lines: [] })
})

test("work approval belongs to an unfinished work in the same Bot, target device and session", () => {
  const [work] = parseOpenWorks({ works: [{ work_id: "new", conversation_id: "chat", trigger: "m", target_node: "worker", cwd: "/w", title: "新任务", prompt: "p", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, thread_id: "s" }] })
  const [approval] = parseQuestions({ questions: [row("approval", 2, { about: { ...about, request_id: "request" } })] })
  const input = { work, conversationId: "chat", questions: [approval], answered: {} }
  assert.equal(workApproval(input), approval)
  assert.equal(workApproval({ ...input, work: { ...work, work_id: "old", state: "finished", outcome: "completed" } }), undefined)
  assert.equal(workApproval({ ...input, work: { ...work, state: "finished" } }), undefined)
  assert.equal(workApproval({ ...input, answered: { approval: "继续" } }), undefined)
  assert.equal(workApproval({ ...input, conversationId: "other-bot" }), undefined)
  assert.equal(workApproval({ ...input, work: { ...work, target_node: "other-device" } }), undefined)
  assert.equal(workApproval({ ...input, work: { ...work, thread_id: "other-session" } }), undefined)
  assert.equal(workApproval({ ...input, questions: [{ ...approval, about: { ...about, agent: "codex" } }] }), undefined)
  assert.equal(workApproval({ ...input, questions: [{ ...approval, closed: "request_lost" }] }), undefined)
  assert.equal(workApproval({ ...input, questions: [{ ...approval, answer: { message_id: "a", text: "继续", author: "me", created_at_ms: 3 } }] }), undefined)
})
