import assert from "node:assert/strict"
import test from "node:test"
import { parseChanged } from "../lib/progress.ts"
import { confirmReason, parseConversation, parseQuestions, parseRecords, workChecks } from "./model.ts"
import { mergeBots, mergeConversation, mergeNotifications, mergeOpenWorks, mergeQuestions } from "./records.ts"
import type { NodeInfo } from "./types.ts"

const rank = (lamport: number) => `${String(lamport).padStart(20, "0")}:${"0".repeat(20)}:h${lamport}`
const message = (message_id: string, lamport: number) => ({ message_id, role: "user", text: message_id, event_hash: `e-${message_id}`, created_at_ms: lamport, rank: rank(lamport) })
const work = (work_id: string, extra: Record<string, unknown> = {}) => ({
  work_id, trigger: "t", target_node: "worker", cwd: "/w", title: "", prompt: "p", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, kind: "codex", ...extra,
})
const question = (question_id: string, conversation_id: string | null, answer: unknown = null) => ({
  question_id, conversation_id, bot: null, severity: "info", text: "?", choices: [],
  about: { node: "n", agent: "codex", session_id: "s", cwd: "/", summary: "" }, author: "n", created_at_ms: 1, answer,
})
const shown = parseConversation({
  conversation: { conversation_id: "chat", title: "", owner: "gateway", owner_epoch: 1 },
  messages: [message("m1", 1), message("m3", 3)], works: [work("w1"), work("w2")],
  deliveries: [{ state: "dispatch_waiting", work_id: "w9", target_node: "worker", cwd: "/w" }, { state: "stop_waiting", work_id: "w1", target_node: "worker" }],
  scheduler: { jobs: [{ trigger: "a", state: "queued" }] }, checks: [], local_node: "gateway", changes: [], bot_settings: { skills: [], schedules: [], memory: [] }, sent: [],
  questions: [], sessions: [],
}, "chat")
const records = (extra: Record<string, unknown> = {}) => {
  const event = parseChanged({
    type: "records", conversation_id: "chat", messages: [], works: [], last_message: null, questions: [], notifications: [],
    deliveries: [], checks: [], jobs: [], sent: [], ...extra,
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
  assert.equal(next.questions.length, 1)
  const again = mergeConversation(next, records({ messages: [message("m2", 2)] }))
  assert.equal(again.messages.length, 3, "a repeated push does not duplicate the message")
})

test("pushed records update the network-wide lists for their conversation only", () => {
  const pushed = records({
    conversation_id: "chat", questions: [question("open", "chat"), question("done", "chat", { message_id: "a", text: "好", author: "b", created_at_ms: 2 })],
    notifications: [{ notification_id: "n2", kind: "question", text: "?", conversation_id: "chat" }],
    last_message: { text: "hi", role: "assistant", created_at_ms: 5 },
  })
  const questions = mergeQuestions(parseQuestions({ questions: [question("old", "chat"), question("other", "other"), question("session", null)] }), pushed)
  assert.deepEqual(questions.map((item) => item.question_id), ["other", "session", "open"], "answered ones leave the list")
  assert.deepEqual(mergeQuestions(questions, { ...pushed, conversation_id: "sessions", questions: [] }).map((item) => item.question_id), ["other", "open"])
  const notifications = mergeNotifications([{ notification_id: "n1", kind: "question", text: "", conversation_id: "chat" }, { notification_id: "x", kind: "question", text: "", conversation_id: "other" }], pushed)
  assert.deepEqual(notifications.map((item) => item.notification_id), ["x", "n2"])
  const bots = mergeBots([{ bot_id: "b", conversation_id: "chat", name: "", instructions: "", event_hash: "", last_message: null }], pushed)
  assert.equal(bots[0].last_message?.text, "hi")
})

test("a work waits for confirmation only once its device cannot report it", () => {
  const node = (online: boolean, codex_connected?: boolean): NodeInfo[] => [{ device_id: "worker", name: "", local: false, online, revoked: false, capabilities: { manager: null, relay: false, roots: [], codex_connected } }]
  const running = records({ works: [work("w1")] }).works[0]
  assert.equal(confirmReason(running, []), null, "an unknown device is not judged")
  assert.equal(confirmReason(running, node(true, true)), null)
  assert.equal(confirmReason(running, node(false)), "offline")
  assert.equal(confirmReason(running, node(true, false)), "disconnected")
  assert.equal(confirmReason({ ...running, kind: "task" }, node(true, false)), null, "a task does not need the App Server")
  assert.equal(confirmReason({ ...running, state: "unverified", outcome: "unverified" }, node(true, true)), "unverified")
  let open = mergeOpenWorks([], records({ works: [work("w1")] }))
  assert.deepEqual(workChecks(open, node(false)).map((item) => [item.check_id, item.conversation_id, item.reason]), [["work:w1", "chat", "offline"]])
  open = mergeOpenWorks(open, records({ works: [work("w1", { state: "finished", outcome: "interrupted" })] }))
  assert.equal(open.length, 0, "a settled work leaves the list")
})

test("a change the Node cannot map reloads only its conversation", () => {
  assert.deepEqual(parseChanged({ type: "changed", conversation_id: "chat" }), { type: "invalidate", conversation_id: "chat" })
  assert.deepEqual(parseChanged(undefined), { type: "invalidate" })
})
