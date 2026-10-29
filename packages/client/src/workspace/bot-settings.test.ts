import assert from "node:assert/strict"
import test from "node:test"
import { formatIn, formatWhen, lineDiff, placeChanges, scheduleDetail, scheduleFrequency, scheduleName, scheduleTrigger, sortSchedules } from "./bot-settings.ts"
import { parseBots, parseConversation } from "./model.ts"
import type { BotChangeRecord } from "./types.ts"

const at = (day: number, hour: number, minute = 0) => new Date(2026, 9, day, hour, minute).getTime()

test("schedule frequency reads as plain words in local time", () => {
  const first = at(20, 9, 5)
  const weekday = "日一二三四五六"[new Date(first).getDay()]
  assert.equal(scheduleFrequency({ first_run_at_ms: first }), "一次")
  assert.equal(scheduleFrequency({ first_run_at_ms: first, every_minutes: 1440 }), "每天 09:05")
  assert.equal(scheduleFrequency({ first_run_at_ms: first, every_minutes: 10080 }), `每周${weekday} 09:05`)
  assert.equal(scheduleFrequency({ first_run_at_ms: first, every_minutes: 2880 }), "每 2 天 09:05")
  assert.equal(scheduleFrequency({ first_run_at_ms: first, every_minutes: 120 }), "每 2 小时")
  assert.equal(scheduleFrequency({ first_run_at_ms: first, every_minutes: 45 }), "每 45 分钟")
})

test("dates read relative to today", () => {
  assert.equal(formatWhen(at(20, 21, 30), at(20, 8)), "今天 21:30")
  assert.equal(formatWhen(at(21, 9), at(20, 23)), "明天 09:00")
  assert.equal(formatWhen(at(25, 9), at(20, 8)), "10月25日 09:00")
  assert.equal(formatWhen(new Date(2027, 0, 2, 9).getTime(), at(20, 8)), "2027年1月2日 09:00")
})

test("next runs read as a short time from now", () => {
  const now = at(20, 8)
  assert.equal(formatIn(now - 1000, now), "马上")
  assert.equal(formatIn(now + 30_000, now), "1 分钟后")
  assert.equal(formatIn(now + 3 * 3_600_000, now), "3 小时后")
  assert.equal(formatIn(now + 2 * 86_400_000, now), "2 天后")
})

test("schedules sort by next run with paused ones last", () => {
  const rows = [
    { id: "paused", enabled: false, next_run_at_ms: 1 },
    { id: "later", enabled: true, next_run_at_ms: 300 },
    { id: "none", enabled: true, next_run_at_ms: null },
    { id: "soon", enabled: true, next_run_at_ms: 200 },
    { id: "paused2", enabled: false, next_run_at_ms: null },
  ]
  assert.deepEqual(sortSchedules(rows).map((row) => row.id), ["soon", "later", "none", "paused", "paused2"])
})

test("line diff lists only removed and added lines in order", () => {
  assert.deepEqual(lineDiff("a\nb\nc", "a\nB\nc\nd"), [
    { kind: "removed", text: "b" }, { kind: "added", text: "B" }, { kind: "added", text: "d" },
  ])
  assert.deepEqual(lineDiff("", "用英文回复"), [{ kind: "added", text: "用英文回复" }])
  assert.deepEqual(lineDiff("same\n\nx", "same\nx"), [])
})

test("schedule replies carry their schedule id", () => {
  assert.equal(scheduleTrigger("schedule:r-1:1760000000000"), "r-1")
  assert.equal(scheduleTrigger("schedule:a:b:1"), "a:b")
  assert.equal(scheduleTrigger("routine:r-1:1"), null)
  assert.equal(scheduleTrigger("bot-message-1"), null)
  assert.equal(scheduleTrigger(undefined), null)
})

const scheduleChange = (cause: string | null, name: string, at = 1): BotChangeRecord => ({
  change_id: `c-${cause}-${name}`, conversation_id: "chat", cause: cause === null ? null : { conversation_id: "chat", message_id: cause }, event_hash: `h-${cause}-${name}`, created_at_ms: at,
  change: { kind: "schedule", schedule_id: "r", before: null, after: { name, prompt: "p", first_run_at_ms: 0, every_minutes: 60, enabled: true } },
})

test("changes sit after the reply to their message, else after the message, else by time", () => {
  const messages = [
    { message_id: "u1", role: "user", created_at_ms: 10 }, { message_id: "a1", role: "assistant", in_reply_to: "u1", created_at_ms: 20 },
    { message_id: "u2", role: "user", created_at_ms: 30 },
  ]
  const first = scheduleChange("u1", "x"), second = scheduleChange("u1", "y"), third = scheduleChange("u2", "z"), lost = scheduleChange("gone", "w", 25), uncaused = scheduleChange(null, "v", 5), later = scheduleChange(null, "u", 35)
  const placed = placeChanges(messages, [first, third, second, lost, uncaused, later])
  assert.deepEqual(placed.after.get("a1"), [[first, second], [lost]])
  assert.deepEqual(placed.after.get("u2"), [[third], [later]])
  assert.equal(placed.after.has("u1"), false)
  assert.deepEqual(placed.first, [[uncaused]])
  assert.equal(scheduleName("r", { skills: [], schedules: [], memory: [] }, [first, second]), "y")
  assert.equal(scheduleName("other", undefined), "定时任务")
})

test("conversation snapshots parse bot changes strictly", () => {
  const base = {
    conversation: { conversation_id: "chat", title: "t", owner: null, owner_epoch: 0 }, messages: [],
    works: [], deliveries: [], checks: [], local_node: "n", questions: [], sessions: [],
    sent: [],
    bot_settings: {
      skills: [{ skill_id: "s", name: "发布", description: "发版步骤" }],
      schedules: [
        { schedule_id: "r", name: "看依赖", prompt: "p", first_run_at_ms: 1, every_minutes: 1440, enabled: false, next_run_at_ms: null },
        { schedule_id: "o", name: "提醒", prompt: "p", first_run_at_ms: 5, enabled: true, depth: 1, next_run_at_ms: 5 },
      ],
      memory: [{ memory_id: "m", text: "用中文" }],
    },
    changes: [
      { change_id: "c1", conversation_id: "chat", cause: { conversation_id: "chat", message_id: "u1" }, event_hash: "h1", created_at_ms: 2, change: { kind: "profile", name: { before: "旧", after: "新" } } },
      { change_id: "c2", conversation_id: "chat", cause: { conversation_id: "chat", message_id: "u1" }, event_hash: "h2", created_at_ms: 2, change: { kind: "skill", skill_id: "s", before: null, after: { name: "发布", description: "d", body: "b" } } },
      { change_id: "c3", conversation_id: "chat", cause: null, event_hash: "h3", created_at_ms: 2, change: { kind: "memory", memory_id: "m", before: null, after: "用中文" } },
      { change_id: "c4", conversation_id: "chat", cause: { conversation_id: "other", message_id: "u9" }, event_hash: "h4", created_at_ms: 2, change: { kind: "schedule", schedule_id: "o", before: null, after: { name: "提醒", prompt: "p", first_run_at_ms: 5, enabled: true, depth: 1 } } },
    ],
  }
  const parsed = parseConversation(base, "chat")
  assert.deepEqual(parsed.changes[0]?.change, { kind: "profile", name: { before: "旧", after: "新" } })
  assert.deepEqual(parsed.changes[2], { change_id: "c3", conversation_id: "chat", cause: null, event_hash: "h3", created_at_ms: 2, change: { kind: "memory", memory_id: "m", before: null, after: "用中文" } })
  assert.deepEqual(parsed.changes[3]?.cause, { conversation_id: "other", message_id: "u9" })
  assert.deepEqual(parsed.changes[3]?.change, { kind: "schedule", schedule_id: "o", before: null, after: { name: "提醒", prompt: "p", first_run_at_ms: 5, enabled: true, depth: 1 } })
  assert.equal(parsed.bot_settings.schedules[0]?.next_run_at_ms, null)
  assert.equal(parsed.bot_settings.schedules[0]?.every_minutes, 1440)
  assert.equal("every_minutes" in parsed.bot_settings.schedules[1]!, false)
  assert.equal(scheduleFrequency(parsed.bot_settings.schedules[1]!), "一次")
  assert.deepEqual(parsed.bot_settings.memory, [{ memory_id: "m", text: "用中文" }])
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[2], change: { kind: "memory", memory_id: "m", before: null, after: null } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, bot_settings: { ...base.bot_settings, schedules: [{ ...base.bot_settings.schedules[0], every_minutes: 0 }] } }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...base, bot_settings: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "profile" } }] }, "chat"))
  const configured = { kind: "profile", model: { provider: "codex", before: "", after: "gpt-x" }, skills: { before: null, after: ["s"] } }
  assert.deepEqual(parseConversation({ ...base, changes: [{ ...base.changes[0], change: configured }] }, "chat").changes[0]?.change, configured)
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "profile", model: { before: "", after: "gpt-x" } } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "model" } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[1], change: { kind: "skill", skill_id: "s", before: null, after: null } }] }, "chat"))
  assert.deepEqual(parseConversation({ ...base, sent: [{ message_id: "bm", to: "ops", text: "看看备份", created_at_ms: 7 }] }, "chat").sent,
    [{ message_id: "bm", to: "ops", text: "看看备份", created_at_ms: 7 }])
})

test("a reminder whose note is its name shows the sentence once", () => {
  assert.equal(scheduleDetail({ name: "看看部署", prompt: " 看看部署\n" }), null)
  assert.equal(scheduleDetail({ name: "看看部署", prompt: "看看部署\n顺便查日志" }), "看看部署\n顺便查日志")
  assert.equal(scheduleDetail({ name: "每日巡检", prompt: "检查依赖更新" }), "检查依赖更新")
})

test("a bot lists its model per provider and its skills, null meaning every skill", () => {
  const bot = { bot_id: "b", conversation_id: "c", name: "Ops", instructions: "", event_hash: "h", last_message: null }
  const [all, some] = parseBots({ bots: [{ ...bot, models: {}, skills: null }, { ...bot, models: { cursor: "gpt-x" }, skills: ["s"] }] })
  assert.deepEqual([all?.models, all?.skills, some?.models, some?.skills], [{}, null, { cursor: "gpt-x" }, ["s"]])
  assert.throws(() => parseBots({ bots: [{ ...bot, models: { codex_sdk: "x" }, skills: null }] }))
})

test("a bot's picked face parses, none means the face its id gives, and a hue outside the set is refused", () => {
  const bot = { bot_id: "b", conversation_id: "c", name: "Ops", instructions: "", event_hash: "h", last_message: null, models: {}, skills: null }
  const avatar = { outline: "cloud", eyes: "wide", hue: 345 }
  const [picked, plain] = parseBots({ bots: [{ ...bot, avatar }, { ...bot, avatar: null }] })
  assert.deepEqual([picked?.avatar, plain?.avatar], [avatar, null])
  assert.throws(() => parseBots({ bots: [{ ...bot, avatar: { ...avatar, hue: 70 } }] }))
})
