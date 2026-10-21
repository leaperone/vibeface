import assert from "node:assert/strict"
import test from "node:test"
import { parseAnswer, parseConversation, parseQuestions, parseTriageSettings } from "./model.ts"
import { answerBody, answerScope, foldAnswers, openQuestions, placeByTime, projectName, settledAnswer } from "./questions.ts"

const about = { node: "worker", agent: "codex", session_id: "s", cwd: "/Users/me/web/", summary: "改登录" }
const row = (question_id: string, created_at_ms: number, extra: Record<string, unknown> = {}) => ({
  question_id, conversation_id: "chat", assistant: { bot_id: "bot", name: "助手" }, severity: "attention", text: "要不要继续",
  choices: [{ label: "继续" }, { label: "看看", conversation_id: "other" }], about, author: "gateway", created_at_ms, answer: null, ...extra,
})
const snapshot = (extra: Record<string, unknown>) => ({
  conversation: { conversation_id: "chat", title: "助手", owner: "gateway", owner_epoch: 1 }, messages: [], works: [], deliveries: [],
  checks: [], local_node: "gateway", changes: [], assistant: { skills: [], routines: [] }, questions: [], sessions: [], ...extra,
})

test("questions parse from the list and the chat snapshot, and fail closed on shapes the web cannot show", () => {
  const session = row("s1", 2, { conversation_id: null, assistant: null, about: { ...about, request_id: "r" } })
  const [question, parsedSession] = parseQuestions({ questions: [row("q1", 1), session] })
  assert.equal(question.choices[1].conversation_id, "other")
  assert.equal(parsedSession.conversation_id, null, "a session nobody's assistant started has no chat")
  const { choices: _, ...open } = row("q2", 3)
  assert.deepEqual(parseQuestions({ questions: [open] })[0].choices, [], "a question the node sent without choices is free text only")
  assert.equal(parsedSession.assistant, null)
  assert.equal(parsedSession.about.request_id, "r")
  const answered = row("q2", 3, { answer: { message_id: "a-q2", text: "继续", author: "client", created_at_ms: 4 } })
  const chat = parseConversation(snapshot({
    questions: [answered],
    sessions: [{ node: "worker", agent: "codex", session_id: "s", cwd: "/w", summary: "改登录", question_id: "q2", created_at_ms: 2 }],
    messages: [{ message_id: "a-q2", role: "user", text: "继续", author: "client", created_at_ms: 4, event_hash: "h", answers: "q2" }],
  }))
  assert.equal(chat.questions[0].answer?.text, "继续", "answered questions stay in the chat")
  assert.equal(chat.sessions[0].question_id, "q2")
  assert.equal(chat.messages[0].answers, "q2")
  for (const bad of [{ severity: "loud" }, { about: null }, { answer: { text: "继续" } }, { choices: [{}] }, { conversation_id: 1 }])
    assert.throws(() => parseQuestions({ questions: [row("q", 1, bad)] }), Error, JSON.stringify(bad))
  assert.throws(() => parseConversation(snapshot({ sessions: [{ node: "worker" }] })))
})

test("an answer result is the question's own message, or it is not trusted", () => {
  assert.deepEqual(parseAnswer({ status: "answered", message: { message_id: "a-q1", text: "继续" } }, "q1"), { status: "answered", text: "继续" })
  assert.deepEqual(parseAnswer({ status: "already_answered", message: { message_id: "a-q1", text: "停下" } }, "q1"), { status: "already_answered", text: "停下" })
  assert.throws(() => parseAnswer({ status: "queued", message: { message_id: "a-q1", text: "继续" } }, "q1"), /无法识别/)
  assert.throws(() => parseAnswer({ status: "answered", message: { message_id: "a-q2", text: "继续" } }, "q1"), /对不上/)
})

test("an answer is signed as one message per question, for its chat or for sessions", () => {
  assert.deepEqual(answerBody("q1", "继续"), { type: "message_posted", message_id: "a-q1", role: "user", text: "继续", report_results: true, answers: "q1" })
  assert.equal(answerScope({ conversation_id: "chat" }), "chat")
  assert.equal(answerScope({ conversation_id: null }), "sessions")
})

test("待处理 lists exactly the unanswered questions, newest first", () => {
  const questions = parseQuestions({ questions: [row("old", 1), row("done", 3, { answer: { message_id: "a-done", text: "继续", author: "c", created_at_ms: 4 } }), row("new", 2)] })
  assert.deepEqual(openQuestions(questions).map((question) => question.question_id), ["new", "old"])
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
  assert.equal(projectName("/Users/me/web/"), "web")
  assert.equal(projectName("C:\\code\\api"), "api")
})

test("questions go to the web, or to the web and Grok; nothing else", () => {
  const settings = (default_outlet: string) => ({ default_outlet, projects: [], webhook_url: null, scan_codex_sessions: true, webhook_secret_set: false, decision_url: "", device_id: "d", decision_tokens: [], outbox: { pending: 0, delivered: 0, dead: 0 } })
  assert.equal(parseTriageSettings(settings("both")).default_outlet, "both")
  assert.equal(parseTriageSettings(settings("record")).default_outlet, "record")
  for (const old of ["webhook", "off"]) assert.throws(() => parseTriageSettings(settings(old)), /无法识别/)
})

test("an answer replaced by another side's says so instead of vanishing", () => {
  assert.equal(settledAnswer(undefined, undefined), undefined, "an open question has nothing settled")
  assert.deepEqual(settledAnswer(undefined, "继续"), { text: "继续", elsewhere: false }, "this browser's answer shows before the network has it")
  assert.deepEqual(settledAnswer("继续", "继续"), { text: "继续", elsewhere: false })
  assert.deepEqual(settledAnswer("停下", undefined), { text: "停下", elsewhere: false }, "an answer given elsewhere with nothing tried here")
  assert.deepEqual(settledAnswer("停下", "继续"), { text: "停下", elsewhere: true }, "the other side's answer won the race")
})
