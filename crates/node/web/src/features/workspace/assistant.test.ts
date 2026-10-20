import assert from "node:assert/strict"
import test from "node:test"
import { formatWhen, lineDiff, nextRun, placeChanges, routineFrequency, routineName, routineTrigger } from "./assistant.ts"
import { parseConversation } from "./model.ts"
import type { AssistantChangeRecord } from "./types.ts"

const at = (day: number, hour: number, minute = 0) => new Date(2026, 9, day, hour, minute).getTime()

test("routine frequency reads as plain words in local time", () => {
  const first = at(20, 9, 5)
  const weekday = "日一二三四五六"[new Date(first).getDay()]
  assert.equal(routineFrequency({ first_run_at_ms: first, every_minutes: 1440 }), "每天 09:05")
  assert.equal(routineFrequency({ first_run_at_ms: first, every_minutes: 10080 }), `每周${weekday} 09:05`)
  assert.equal(routineFrequency({ first_run_at_ms: first, every_minutes: 2880 }), "每 2 天 09:05")
  assert.equal(routineFrequency({ first_run_at_ms: first, every_minutes: 120 }), "每 2 小时")
  assert.equal(routineFrequency({ first_run_at_ms: first, every_minutes: 45 }), "每 45 分钟")
})

test("next run skips missed slots and dates read relative to today", () => {
  const routine = { first_run_at_ms: at(18, 9), every_minutes: 1440 }
  assert.equal(nextRun(routine, at(17, 12)), at(18, 9))
  assert.equal(nextRun(routine, at(20, 9)), at(20, 9))
  assert.equal(nextRun(routine, at(20, 9, 1)), at(21, 9))
  assert.equal(formatWhen(at(20, 21, 30), at(20, 8)), "今天 21:30")
  assert.equal(formatWhen(at(21, 9), at(20, 23)), "明天 09:00")
  assert.equal(formatWhen(at(25, 9), at(20, 8)), "10月25日 09:00")
  assert.equal(formatWhen(new Date(2027, 0, 2, 9).getTime(), at(20, 8)), "2027年1月2日 09:00")
})

test("line diff lists only removed and added lines in order", () => {
  assert.deepEqual(lineDiff("a\nb\nc", "a\nB\nc\nd"), [
    { kind: "removed", text: "b" }, { kind: "added", text: "B" }, { kind: "added", text: "d" },
  ])
  assert.deepEqual(lineDiff("", "用英文回复"), [{ kind: "added", text: "用英文回复" }])
  assert.deepEqual(lineDiff("same\n\nx", "same\nx"), [])
})

test("routine replies carry their routine id", () => {
  assert.equal(routineTrigger("routine:r-1:1760000000000"), "r-1")
  assert.equal(routineTrigger("routine:a:b:1"), "a:b")
  assert.equal(routineTrigger("m-1"), null)
  assert.equal(routineTrigger(undefined), null)
})

const routineChange = (cause: string, name: string): AssistantChangeRecord => ({
  change_id: `c-${cause}-${name}`, cause, event_hash: `h-${cause}-${name}`, created_at_ms: 1,
  change: { kind: "routine", routine_id: "r", before: null, after: { name, prompt: "p", first_run_at_ms: 0, every_minutes: 60, enabled: true } },
})

test("changes sit after the reply to their message, else after the message", () => {
  const messages = [
    { message_id: "u1", role: "user" }, { message_id: "a1", role: "assistant", in_reply_to: "u1" },
    { message_id: "u2", role: "user" },
  ]
  const first = routineChange("u1", "x"), second = routineChange("u1", "y"), third = routineChange("u2", "z"), lost = routineChange("gone", "w")
  const { after, rest } = placeChanges(messages, [first, third, second, lost])
  assert.deepEqual(after.get("a1"), [first, second])
  assert.deepEqual(after.get("u2"), [third])
  assert.equal(after.has("u1"), false)
  assert.deepEqual(rest, [[lost]])
  assert.equal(routineName("r", { skills: [], routines: [] }, [first, second]), "y")
  assert.equal(routineName("other", undefined), "例行任务")
})

test("conversation snapshots parse assistant changes strictly", () => {
  const base = {
    conversation: { conversation_id: "chat", title: "t", owner: null, owner_epoch: 0 }, messages: [], autonomy: { mode: "report", version: 0 },
    works: [], pending_checks: [], local_node: "n",
    assistant: { skills: [{ skill_id: "s", name: "发布", description: "发版步骤" }], routines: [{ routine_id: "r", name: "看依赖", prompt: "p", first_run_at_ms: 1, every_minutes: 1440, enabled: false, next_run_at_ms: null }] },
    changes: [
      { change_id: "c1", cause: "u1", event_hash: "h1", created_at_ms: 2, change: { kind: "profile", name: { before: "旧", after: "新" } } },
      { change_id: "c2", cause: "u1", event_hash: "h2", created_at_ms: 2, change: { kind: "skill", skill_id: "s", before: null, after: { name: "发布", description: "d", body: "b" } } },
    ],
  }
  const parsed = parseConversation(base, "chat")
  assert.deepEqual(parsed.changes[0]?.change, { kind: "profile", name: { before: "旧", after: "新" } })
  assert.equal(parsed.assistant.routines[0]?.next_run_at_ms, null)
  assert.throws(() => parseConversation({ ...base, changes: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...base, assistant: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "profile" } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "model" } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[1], change: { kind: "skill", skill_id: "s", before: null, after: null } }] }, "chat"))
})
