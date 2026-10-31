import assert from "node:assert/strict"
import test from "node:test"
import { formatIn, formatWhen, lineDiff, overwriteCopy, placeChanges, restoreRequest, scheduleDetail, scheduleFrequency, scheduleName, scheduleTrigger, sortSchedules } from "./bot-settings.ts"
import { parseBots, parseConversation } from "./model.ts"
import type { BotChangeRecord, MemoryMetadata } from "./types.ts"
import type { OutboxItem, OutboxStore } from "../lib/outbox.ts"
import { requestKey } from "../lib/secure.ts"
import { DATA_EPOCH, memoryBase, memoryController, memoryResultLabel, memorySaved, parseMemoryControlResult, parseMemoryStateResult } from "./memory.ts"

const at = (day: number, hour: number, minute = 0) => new Date(2026, 9, day, hour, minute).getTime()

const memoryMetadata: MemoryMetadata = { epoch: DATA_EPOCH, revision: 'a'.repeat(64), control_revision: 1, source_event_hash: 'c'.repeat(64), origin: 'user', user_controlled: true, essential: true }
const memoryState = (revision = memoryMetadata.revision, text: string | null = '现有记忆') => ({ memory_id: 'memory', text, metadata: { ...memoryMetadata, revision } })
const operationId = 'e'.repeat(64)
function memoryClient(rpc: (method: string, params: Record<string, unknown>) => Promise<unknown>) {
  const items = new Map<string, OutboxItem>(), signed: Record<string, unknown>[] = []
  const outbox: OutboxStore = {
    all: async () => structuredClone([...items.values()]),
    put: async (item) => { items.set(item.key, structuredClone(item)) },
    delete: async (key) => { items.delete(key) },
  }
  const client = memoryController({ outbox, rpc, key: (conversationId, memoryId) => JSON.stringify([conversationId, memoryId]), sign: async (_conversationId, body) => {
    signed.push(body)
    return { operation_id: operationId, attestation: { device_id: 'b'.repeat(64), signature: 'original-signature' } }
  } })
  return { ...client, outbox, signed }
}

test('Memory request key matches the Rust canonical payload vector', async () => {
  const payload = new TextEncoder().encode('{"body":{"after":"用中文","base":{"epoch":11,"version":null},"memory_id":"memory","type":"memory_controlled"},"network_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","scope_id":"chat"}')
  assert.equal(await requestKey(payload, 'b'.repeat(64)), '71e91d1340a1a11d26fa2c3fcba63cac764deec8d617be3518dcb71e6d040e49')
  assert.notEqual(await requestKey(payload, 'c'.repeat(64)), await requestKey(payload, 'b'.repeat(64)))
})

test('Memory create signs only the frozen body and accepts the matching current receipt', async () => {
  const sent: unknown[] = []
  const client = memoryClient(async (method, params) => {
    sent.push({ method, params })
    return { state: 'applied', operation_id: operationId, current: memoryState(operationId, '用中文') }
  })
  const base = memoryBase(null)
  const result = await client.controlMemory('chat', 'memory', base, '用中文')
  assert.deepEqual(client.signed, [{ type: 'memory_controlled', memory_id: 'memory', base: { epoch: DATA_EPOCH, version: null }, after: '用中文' }])
  assert.deepEqual(sent, [{ method: 'control_memory', params: { conversation_id: 'chat', memory_id: 'memory', base, after: '用中文', operation_id: operationId, attestation: { device_id: 'b'.repeat(64), signature: 'original-signature' } } }])
  assert.equal(memorySaved(result), true)
  assert.equal(memoryResultLabel(result), '记忆已保存。')
  assert.deepEqual(await client.outbox.all(), [])
})

test('Memory deletion signs the observed base and retains the tombstone in its result', async () => {
  const client = memoryClient(async () => ({ state: 'applied', operation_id: operationId, current: memoryState(operationId, null) }))
  await assert.rejects(client.controlMemory('chat', 'memory', memoryBase(null), null), /需要当前版本/)
  assert.equal(client.signed.length, 0)
  const base = memoryBase(memoryMetadata)
  const result = await client.controlMemory('chat', 'memory', base, null)
  assert.deepEqual(client.signed, [{ type: 'memory_controlled', memory_id: 'memory', base, after: null }])
  assert.equal(result.current?.text, null)
  assert.equal(memoryResultLabel(result), '记忆已删除。')
})

test('a malformed Memory write receipt remains unknown until the original operation is read back', async () => {
  const client = memoryClient(async (method) => method === 'control_memory'
    ? { state: 'applied', operation_id: 'f'.repeat(64), current: memoryState(operationId) }
    : { receipt: 'recorded', current: memoryState(operationId) })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'), /原操作不一致/)
  assert.equal((await client.outbox.all()).length, 1)
  assert.equal((await client.controlMemory('chat', 'memory', base, '原草稿')).state, 'already_recorded')
  assert.equal(client.signed.length, 1)
  assert.deepEqual(await client.outbox.all(), [])
})

test('an unknown Memory write stays byte-for-byte when reconciliation fails or another draft is submitted', async () => {
  const calls: string[] = []
  const client = memoryClient(async (method) => { calls.push(method); throw new Error('disconnected') })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'), /disconnected/)
  const original = await client.outbox.all()
  await assert.rejects(client.controlMemory('chat', 'memory', base, '新草稿'), /disconnected/)
  assert.deepEqual(await client.outbox.all(), original)
  assert.equal(client.signed.length, 1)
  assert.deepEqual(calls, ['control_memory', 'memory_state'])
})

test('an entrance not seeing the operation preserves its unknown outbox when the base changed, including a tombstone', async () => {
  let query = false
  const client = memoryClient(async (method, params) => {
    if (method === 'control_memory') throw new Error('timeout')
    assert.deepEqual(params, { conversation_id: 'chat', memory_id: 'memory', operation_id: operationId })
    query = true
    return { receipt: 'not_seen_here', current: memoryState('f'.repeat(64), null) }
  })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'))
  const original = await client.outbox.all()
  const result = await client.controlMemory('chat', 'memory', base, '原草稿')
  assert.equal(query, true)
  assert.equal(result.state, 'conflict')
  assert.equal(result.current?.text, null)
  assert.equal(memorySaved(result), false)
  assert.deepEqual(await client.outbox.all(), original)
  await assert.rejects(client.controlMemory('chat', 'memory', memoryBase(result.current?.metadata ?? null), '新草稿'), /草稿已保留/)
  assert.equal(client.signed.length, 1)
  assert.deepEqual(await client.outbox.all(), original)
})

test('Memory retry sends its original attestation after reconciliation despite a different carrier representative', async () => {
  const writes: Record<string, unknown>[] = []
  const client = memoryClient(async (method, params) => {
    if (method === 'memory_state') return { receipt: 'not_seen_here', current: { ...memoryState(), metadata: { ...memoryMetadata, source_event_hash: 'd'.repeat(64) } } }
    writes.push(structuredClone(params))
    if (writes.length === 1) throw new Error('timeout')
    return { state: 'applied', operation_id: operationId, current: memoryState(operationId) }
  })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'))
  assert.equal((await client.controlMemory('chat', 'memory', base, '原草稿')).state, 'applied')
  assert.deepEqual(writes[1], writes[0])
  assert.equal(client.signed.length, 1)
  assert.deepEqual(await client.outbox.all(), [])
})

test('a replayed Memory request that now conflicts cannot erase the original unknown operation', async () => {
  let writes = 0
  const client = memoryClient(async (method) => {
    if (method === 'memory_state') return { receipt: 'not_seen_here', current: memoryState() }
    if (++writes === 1) throw new Error('timeout')
    return { state: 'conflict', operation_id: operationId, current: memoryState('f'.repeat(64)) }
  })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'))
  const original = await client.outbox.all()
  assert.equal((await client.controlMemory('chat', 'memory', base, '原草稿')).state, 'conflict')
  assert.equal(client.signed.length, 1)
  assert.deepEqual(await client.outbox.all(), original)
})

test('a superseded Memory receipt clears only the known operation and never reports saved', async () => {
  const calls: string[] = []
  const client = memoryClient(async (method) => {
    calls.push(method)
    if (method === 'control_memory') throw new Error('timeout')
    return { receipt: 'superseded', current: memoryState('f'.repeat(64)) }
  })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'))
  const result = await client.controlMemory('chat', 'memory', base, '原草稿')
  assert.equal(result.state, 'superseded')
  assert.equal(memorySaved(result), false)
  assert.match(memoryResultLabel(result), /已被其他修改替代.*草稿已保留/)
  assert.deepEqual(calls, ['control_memory', 'memory_state'])
  assert.deepEqual(await client.outbox.all(), [])
  assert.equal(client.signed.length, 1)
})

test('a recorded old Memory draft cannot be mistaken for saving the new draft', async () => {
  const client = memoryClient(async (method) => {
    if (method === 'control_memory') throw new Error('timeout')
    return { receipt: 'recorded', current: memoryState(operationId, '原草稿') }
  })
  const base = memoryBase(memoryMetadata)
  await assert.rejects(client.controlMemory('chat', 'memory', base, '原草稿'))
  await assert.rejects(client.controlMemory('chat', 'memory', base, '新草稿'), /草稿已保留/)
  assert.equal(client.signed.length, 1)
  assert.deepEqual(await client.outbox.all(), [])
})

test('Memory state, result and metadata fail closed on malformed or mismatched receipts', () => {
  const result = { state: 'applied', operation_id: operationId, current: memoryState(operationId) }
  assert.equal(parseMemoryControlResult(result, 'memory', operationId).state, 'applied')
  assert.throws(() => parseMemoryControlResult({ ...result, operation_id: 'f'.repeat(64) }, 'memory', operationId))
  assert.throws(() => parseMemoryControlResult({ ...result, current: memoryState() }, 'memory', operationId))
  assert.throws(() => parseMemoryControlResult({ ...result, current: { ...result.current, memory_id: 'other' } }, 'memory', operationId))
  for (const invalid of [{ control_revision: undefined }, { control_revision: -1 }, { control_revision: 0x1_0000_0000 }, { source_event_hash: null }, { origin: 'import' }, { epoch: DATA_EPOCH - 1 }]) {
    assert.throws(() => parseMemoryControlResult({ ...result, current: { ...result.current, metadata: { ...result.current.metadata, ...invalid } } }, 'memory', operationId))
  }
  assert.throws(() => parseMemoryControlResult({ ...result, state: 'success' }, 'memory', operationId))
  assert.throws(() => parseMemoryStateResult({ current: result.current }, 'memory', operationId))
  assert.throws(() => parseMemoryStateResult({ current: result.current, receipt: null }, 'memory', operationId))
  assert.throws(() => parseMemoryStateResult({ current: memoryState(), receipt: 'recorded' }, 'memory', operationId))
  assert.throws(() => parseMemoryStateResult({ current: result.current, receipt: 'superseded' }, 'memory', operationId))
  assert.deepEqual(parseMemoryStateResult({ current: memoryState('f'.repeat(64), null), receipt: null }, 'memory').current?.text, null)
})

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
  assert.equal(scheduleName("r", { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, [first, second]), "y")
  assert.equal(scheduleName("other", undefined), "定时任务")
})

test("conversation snapshots parse bot changes strictly", () => {
  const base = {
    conversation: { conversation_id: "chat", title: "t", owner: null, owner_epoch: 0 }, messages: [],
    works: [], deliveries: [], checks: [], local_node: "n", questions: [], sessions: [],
    sent: [], overwritten: [],
    bot_settings: {
      skills: [{ skill_id: "s", name: "发布", description: "发版步骤" }],
      schedules: [
        { schedule_id: "r", name: "看依赖", prompt: "p", first_run_at_ms: 1, every_minutes: 1440, enabled: false, next_run_at_ms: null },
        { schedule_id: "o", name: "提醒", prompt: "p", first_run_at_ms: 5, enabled: true, depth: 1, next_run_at_ms: 5 },
      ],
      memory: [{ memory_id: "m", text: "用中文", metadata: { epoch: 11, revision: "a".repeat(64), control_revision: 0, source_event_hash: "b".repeat(64), origin: "agent", user_controlled: false, essential: true } }], memory_sync_conflicts: { items: [], has_more: false },
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
  assert.deepEqual(parsed.bot_settings.memory, base.bot_settings.memory)
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[2], change: { kind: "memory", memory_id: "m", before: null, after: null } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, bot_settings: { ...base.bot_settings, schedules: [{ ...base.bot_settings.schedules[0], every_minutes: 0 }] } }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...base, bot_settings: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "profile" } }] }, "chat"))
  const configured = { kind: "profile", model: { provider: "codex", before: "", after: "gpt-x" }, skills: { before: null, after: ["s"] } }
  assert.deepEqual(parseConversation({ ...base, changes: [{ ...base.changes[0], change: configured }] }, "chat").changes[0]?.change, configured)
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "profile", model: { before: "", after: "gpt-x" } } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "model" } }] }, "chat"))
  const faceOnly = { kind: "profile", avatar: { before: null, after: { outline: "cloud", eyes: "wide", hue: 285 } } }
  assert.deepEqual(parseConversation({ ...base, changes: [{ ...base.changes[0], change: faceOnly }] }, "chat").changes[0]?.change, faceOnly)
  const backToDefault = { kind: "profile", avatar: { before: { outline: "cloud", eyes: "wide", hue: 285 }, after: null } }
  assert.deepEqual(parseConversation({ ...base, changes: [{ ...base.changes[0], change: backToDefault }] }, "chat").changes[0]?.change, backToDefault)
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[0], change: { kind: "profile", avatar: { before: null, after: { outline: "cloud", eyes: "wide", hue: 70 } } } }] }, "chat"))
  assert.throws(() => parseConversation({ ...base, changes: [{ ...base.changes[1], change: { kind: "skill", skill_id: "s", before: null, after: null } }] }, "chat"))
  assert.deepEqual(parseConversation({ ...base, sent: [{ message_id: "bm", to: "ops", text: "看看备份", created_at_ms: 7 }] }, "chat").sent,
    [{ message_id: "bm", to: "ops", text: "看看备份", created_at_ms: 7 }])
})

test("a change another device replaced reads as one line and asks for it back the usual way", () => {
  const base = {
    conversation: { conversation_id: "chat", title: "t", owner: null, owner_epoch: 0 }, messages: [], works: [], deliveries: [], checks: [], local_node: "n",
    questions: [], sessions: [], sent: [], changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } },
  }
  assert.deepEqual(parseConversation(base, "chat").overwritten, [])
  const item = (value: Record<string, unknown>) => parseConversation({ ...base, overwritten: [{ change_id: "c1", by: "dev-b", ...value }] }, "chat").overwritten[0]!
  const name = item({ field: "name", value: "Beta" })
  assert.equal(overwriteCopy.line(name, "Ops", "Mac mini"), "Mac mini 把 Ops 的名称改成了「Beta」")
  assert.deepEqual(restoreRequest(name, { kind: "profile", name: { before: "Ops", after: "Alpha" } }), { text: "把名称改回「Alpha」" })
  const model = item({ field: "model", provider: "codex", value: "" })
  assert.equal(overwriteCopy.line(model, "Ops", "Mac mini"), "Mac mini 把 Ops 的 Codex 模型改成了设备默认")
  assert.deepEqual(restoreRequest(model, { kind: "profile", model: { provider: "codex", before: "", after: "gpt-x" } }), { text: "把 Codex 模型改回「gpt-x」" })
  assert.equal(overwriteCopy.line(item({ field: "skills", value: null }), "Ops", "Mac mini"), "Mac mini 把 Ops 的 Skill 改成了全部可用")
  assert.equal(overwriteCopy.line(item({ field: "instructions", value: "\n  先读 README 再动手，改完跑一遍测试再回复我\n第二行" }), "Ops", "Mac mini"),
    "Mac mini 把 Ops 的说明改成了「先读 README 再动手，改完跑一遍测试再回复…」")
  const face = { outline: "cloud", eyes: "wide", hue: 285 } as const
  assert.deepEqual(restoreRequest(item({ field: "avatar", value: null }), { kind: "profile", avatar: { before: null, after: face } }), { avatar: face })
  assert.equal(restoreRequest(name, { kind: "profile", instructions: { before: "", after: "x" } }), null)
  assert.throws(() => item({ field: "project", value: null }))
  assert.throws(() => parseConversation({ ...base, overwritten: undefined }, "chat"))
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
