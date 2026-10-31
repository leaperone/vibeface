import assert from "node:assert/strict"
import test from "node:test"
import { chatTimeline, firstUnread, messageTime, messageTimes, threadReplies, unreadCounts } from "./timeline.ts"

const minute = 60_000
const at = (hour: number, min: number, day = 30) => new Date(2026, 9, day, hour, min, 5).getTime()
const now = at(16, 0)
type Row = { message_id: string; role: string; in_reply_to?: string; created_at_ms: number; rank?: string; event_hash: string }
const message = (message_id: string, rank: number, created_at_ms: number, in_reply_to?: string): Row => ({
  message_id, role: in_reply_to ? "assistant" : "user", in_reply_to, created_at_ms, rank: String(rank).padStart(4, "0"), event_hash: `h-${message_id}`,
})
const ids = (rows: readonly { message_id: string }[]) => rows.map((row) => row.message_id)

// The Node's order after a phone was away: the task m1 started reported back after m2 and m3 were answered.
const late = [
  message("m1", 1, at(15, 59)), message("r1", 2, at(15, 59), "m1"),
  message("m2", 3, at(15, 59)), message("r2", 4, at(15, 59), "m2"),
  message("m3", 5, at(15, 59)), message("r3", 6, at(15, 59), "m3"),
  message("report", 7, at(16, 0), "m1"),
]

test("a task's late report stays where the Node put it, after what was said meanwhile", () => {
  assert.deepEqual(ids(chatTimeline(late).messages), ["m1", "r1", "m2", "r2", "m3", "r3", "report"])
})

test("every device shows one order, whatever order the messages arrived in", () => {
  const shuffled = [late[6], late[2], late[0], late[5], late[1], late[4], late[3]]
  assert.deepEqual(ids(chatTimeline(shuffled).messages), ids(chatTimeline(late).messages))
  const local = { ...message("sending", 0, now), rank: undefined }
  assert.deepEqual(ids(chatTimeline([local, ...late]).messages).at(-1), "sending", "a message the Node has not ranked yet comes last")
})

test("a reply follows the message it answers when the next message was sent before it finished", () => {
  const rows = [
    { message_id: "a", role: "user" },
    { message_id: "b", role: "user" },
    { message_id: "ra", role: "assistant", in_reply_to: "a" },
    { message_id: "rb", role: "assistant", in_reply_to: "b" },
    { message_id: "answer", role: "assistant", in_reply_to: "question-1" },
  ]
  assert.deepEqual(ids(threadReplies(rows)), ["a", "ra", "b", "rb", "answer"])
})

test("a message that joined the running reply sits before that reply", () => {
  const rows = [
    { message_id: "a", role: "user" },
    { message_id: "j1", role: "user" },
    { message_id: "j2", role: "user" },
    { message_id: "ra", role: "assistant", in_reply_to: "a" },
    { message_id: "b", role: "user" },
    { message_id: "rb", role: "assistant", in_reply_to: "b" },
  ]
  assert.deepEqual(ids(threadReplies(rows, new Set(["j1", "j2"]))), ["a", "j1", "j2", "ra", "b", "rb"])
})

test("an aside sits by when it happened in the Node's order, not next to a reply threaded back above it", () => {
  // r1 answers m1 but was written after m2, so it shows above m2; a question asked between m1 and m2 still comes before m2.
  const rows = [message("m1", 1, 10), message("m2", 2, 20), message("r1", 3, 30, "m1")]
  const timeline = chatTimeline(rows, [{ id: "q", created_at_ms: 15 }])
  assert.deepEqual(ids(timeline.messages), ["m1", "r1", "m2"])
  assert.deepEqual(timeline.before.get("m2")?.map((item) => item.id), ["q"])
  assert.equal(timeline.before.get("r1"), undefined)
})

test("新消息 starts at the late report, and the badge counted exactly what follows it", () => {
  const notifications = [
    { notification_id: "h-report", conversation_id: "chat", kind: "manager_response", text: "已看过任务结果。" },
    { notification_id: "work-finished", conversation_id: "chat", kind: "work_finished", text: "完成" },
    { notification_id: "asked", conversation_id: "chat", kind: "question", text: "要继续吗" },
  ]
  const questions = [{ id: "answered", created_at_ms: at(15, 59) - 1, waiting: false }, { id: "open", created_at_ms: now + minute, waiting: true }]
  const counts = unreadCounts(notifications, questions.filter((item) => item.waiting).map(() => ({ conversation_id: "chat" })))
  assert.deepEqual(counts, { chat: 2 }, "a reply and the question waiting on the person; the task and the question are not counted twice")

  const timeline = chatTimeline(late, questions)
  const fresh = new Set(notifications.map((item) => item.notification_id))
  const start = firstUnread(timeline, fresh, (item) => item.waiting)
  assert.equal(start, timeline.messages.at(-1), "the first unread is the late report")
  const shown = [...timeline.messages, ...timeline.rest]
  const after = shown.slice(shown.indexOf(start!))
  assert.equal(after.filter((item) => "event_hash" in item ? fresh.has(item.event_hash) : item.waiting).length, counts.chat)
})

test("a reply written late but threaded above newer messages is still where 新消息 starts", () => {
  const rows = [message("m1", 1, 10), message("m2", 2, 20), message("r1", 3, 30, "m1"), message("r2", 4, 40, "m2")]
  const timeline = chatTimeline(rows)
  assert.equal(firstUnread(timeline, new Set(["h-r1", "h-r2"]), () => false), timeline.messages[1])
  assert.equal(firstUnread(timeline, new Set(), () => false), undefined)
})

test("a question still waiting comes before a later unread reply", () => {
  const timeline = chatTimeline(late, [{ id: "open", created_at_ms: at(15, 59) + 1, waiting: true }])
  assert.deepEqual(firstUnread(timeline, new Set(["h-report"]), (item) => item.waiting), { id: "open", created_at_ms: at(15, 59) + 1, waiting: true })
})

test("one time per minute, under its last message; a message from another minute gets its own", () => {
  const times = messageTimes(chatTimeline(late).messages, now)
  assert.deepEqual([...times], [["r3", "15:59"], ["report", "16:00"]])
  const threaded = chatTimeline([message("m1", 1, at(15, 50)), message("m2", 2, at(15, 51)), message("r1", 3, at(15, 58), "m1")]).messages
  assert.deepEqual([...messageTimes(threaded, now)], [["m1", "15:50"], ["r1", "15:58"], ["m2", "15:51"]], "a reply threaded back up shows its own time")
})

test("a message from another day names the day, and another year the year", () => {
  assert.equal(messageTime(at(9, 7), now), "09:07")
  assert.equal(messageTime(at(23, 59, 29), now), "10月29日 23:59")
  assert.equal(messageTime(new Date(2025, 11, 31, 8, 0).getTime(), now), "2025/12/31 08:00")
  const times = messageTimes([{ message_id: "yesterday", created_at_ms: at(23, 59, 29) }, { message_id: "today", created_at_ms: at(23, 59, 29) + minute }], now)
  assert.deepEqual([...times.values()], ["10月29日 23:59", "00:00"])
})

test("another bot's message counts as unread and starts 新消息", () => {
  const rows = [message("m1", 1, 10), { ...message("b1", 2, 20), role: "bot" as const }]
  const counts = unreadCounts([{ notification_id: "h-b1", conversation_id: "chat", kind: "bot_message", text: "看一下备份" }], [])
  assert.deepEqual(counts, { chat: 1 })
  const timeline = chatTimeline(rows)
  assert.equal(firstUnread(timeline, new Set(["h-b1"]), () => false), timeline.messages[1])
})
