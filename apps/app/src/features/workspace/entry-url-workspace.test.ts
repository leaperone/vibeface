import { createHash } from 'node:crypto'
import { writeBody, ownerBody } from '@vibeface/client/workspace/write-request.ts'
import { decodeOutbox } from '@vibeface/client/workspace/outbox-codec.ts'

import { parseChanged } from '@vibeface/client/lib/progress.ts'
import { completeHistory } from '@vibeface/client/workspace/history-fixture.ts'
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext } from "node:vm"
import { entryAddress, messageOf } from "@vibeface/client/lib/node.ts"
import * as nodeSettings from "@vibeface/client/lib/node.ts"
import type { OutboxItem, OutboxRecord } from '@vibeface/client/lib/outbox.ts'
import type { ReadyConnection } from "@vibeface/client/lib/node.ts"
import type { InputReceipt, InputOptions, InputTarget, PendingMessage, WorkRecord, WorkspaceDomainState } from "@vibeface/client/workspace/types.ts"
import { parseNodes, parseCreateBot, parseBot, formatSize } from "@vibeface/client/workspace/model.ts"
import { botCopy, scheduleName } from "@vibeface/client/workspace/bot-settings.ts"
import { awaitsAnswer, foldAnswers } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { messageKey, replyStatusLabel, inputTargetKey, promptIntent, botReach, botCanSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { chatTimeline, firstUnread, messageTimes, taskMessage } from "@vibeface/client/chat/timeline.ts"
import { accessOf } from "@vibeface/client/identity/access.ts"
import { storageOf } from "@vibeface/client/workspace/storage.ts"
import { mainConversationAttention, unreachable } from "@vibeface/client/workspace/home.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import { inboxItems } from "@vibeface/client/workspace/inbox.ts"

import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'

import { isMessageOutbox, isWorkspaceCommandOutbox, context, componentHarness, componentText, parityNode } from './entry-url-test-kit.test-support.ts'
import * as lastSeenModel from './test-last-seen.test-support.ts'


test("workspace preserves bot and message requests while routing native decisions to their exact work", async (t) => {
  const drainOutbox = async () => { await new Promise(resolve => setTimeout(resolve, 50)) }
  const settleComponent = drainOutbox
  const hooks = hookHarness(), { react, afterRender } = hooks
  const stored = new Map<string, string>()
  const browser = {
    location: { href: "https://node.example/app?conversation=chat" },
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    history: { replaceState(_state: unknown, _title: string, url: string) { browser.location.href = new URL(url, browser.location.href).href } }, setTimeout: () => 1, clearTimeout() {}, setInterval: () => 1, clearInterval() {},
  }
  const sandbox = createContext({
    window: browser, URL, TextEncoder, TextDecoder, Error, crypto: globalThis.crypto, btoa, atob,
    setTimeout: browser.setTimeout, clearTimeout: browser.clearTimeout, setInterval: browser.setInterval, clearInterval: browser.clearInterval,
    structuredClone, navigator: { locks: { request: async (_key: string, _options: unknown, run: (lock: object) => Promise<void>) => run({}) } },
    document: { hidden: false, addEventListener() {}, removeEventListener() {} },
  })
  const fixtureNetwork = 'a'.repeat(64), fixtureClient = 'b'.repeat(64), fixtureEntry = 'c'.repeat(64)
  const signed: string[] = []
  const payloadOf = (network: string, scope: string, body: string) => {
    const event: unknown = JSON.parse(body)
    const data = workspaceModel.record(event)
    assert.equal(typeof data.type, 'string'); assert.equal(data.attestation, undefined)
    return new TextEncoder().encode(`${network}|${scope}|${body}`)
  }
  const signatureOf = (payload: Uint8Array) => createHash('sha512').update(payload).digest('hex')
  const signatureFor = (scope: string, body: Record<string, unknown>) => ({ device_id: fixtureClient, signature: signatureOf(payloadOf(fixtureNetwork, scope, JSON.stringify(body))) })
  const signedItem = (item: OutboxItem): OutboxItem => { const copy = structuredClone(item); copy.params.attestation = signatureFor(item.scope_id, writeBody(item, fixtureClient)); return copy }
  const botBody = (pending: unknown) => {
    const request = parseCreateBot(pending), owner_attestation = signatureFor(request.conversation_id, ownerBody(fixtureEntry, 1))
    const params = { ...request, scheduler_node: fixtureEntry, owner_attestation }
    return JSON.parse(JSON.stringify({ ...params, attestation: signatureFor(request.conversation_id, writeBody({ method: 'create_bot', params }, fixtureClient)) }))
  }
  const outboxItems = new Map<string, OutboxItem>()
  const corruptOutbox = new Map<string, unknown>()
  const memoryOutbox = {
    all: async () => [...outboxItems.values(), ...corruptOutbox.values()].map(item => decodeOutbox(structuredClone(item), workspaceModel.record(item).key)),
    put: async (item: OutboxItem) => { outboxItems.set(item.key, structuredClone(item)) },
    delete: async (key: string) => { outboxItems.delete(key); corruptOutbox.delete(key) },
  }
  const clientRoot = fileURLToPath(new URL("../../../../../packages/client/src/", import.meta.url))
  // Export the real hook only inside this test; production keeps its provider boundary.
  const load = moduleLoader(sandbox, (specifier, filename) => {
    if (specifier === "react") return react
    if (specifier.endsWith("/lib/node.ts")) return { ...nodeSettings, messageOf: (cause: unknown) => cause instanceof Error ? cause.message : String(cause) }
    if (specifier === "react/jsx-runtime") return { jsx: () => null }
    return load(resolve(dirname(filename), specifier))
  }, (filename) => readFileSync(filename, "utf8") + (filename.endsWith("/workspace/context.tsx") ? "\nexport { useWorkspaceState }\n" : ""))
  type Workspace = ReturnType<typeof import("@vibeface/client/workspace/context.tsx").useWorkspace>
  const workspaceModule = load(resolve(clientRoot, "workspace/context.tsx"))
  assert.equal(typeof workspaceModule.useWorkspaceState, "function")
  const { useWorkspaceState } = workspaceModule as { useWorkspaceState(platform: unknown, connection: ReadyConnection): Workspace }
  const platform = {
    conversationCache: false,
    reserveDeliveryIntent: async () => { throw new Error('unexpected delivery reservation') },
    secure: {
      attestationPayload: payloadOf,
      verify(device: string, domain: string, payload: Uint8Array, signature: string) { assert.equal(device, fixtureClient); assert.equal(domain, 'attestation'); assert.equal(signature, signatureOf(payload)) },
    },
    transport: { signClient: async (domain: string, payload: Uint8Array) => { signed.push(`${domain}|${new TextDecoder().decode(payload)}`); return signatureOf(payload) } },
    storage: browser.localStorage, outbox: memoryOutbox, self: "这个浏览器",
    inputLock: <T,>(name: string, save: () => Promise<T>) => globalThis.navigator.locks.request(name, save),
    notifier: { name: "浏览器提醒", permission: async () => "default", request: async () => "default", async show() {} },
    onForeground: () => () => {}, foreground: () => true,
  }
  const bot = { bot_id: "bot", conversation_id: "chat", name: "Bot", instructions: "处理项目", models: {}, skills: null, avatar: null, avatar_version: null, event_hash: "bot-event", last_message: null, busy: null }
  const work: WorkRecord = {
    dispatcher: 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    work_id: "work", trigger: "t", target_node: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", cwd: "/project", title: "工作", prompt: "工作", state: "running", resolved: { provider: "codex", model: null }, summary: "", outcome: null, result_text: "", failure_reason: null }
  let scheduler: Record<string, unknown> | null = null
  let transcript: Record<string, unknown>[] = []
  let conversationMessages: Record<string, Record<string, unknown>[]> | null = null
  const posted = (message_id: unknown) => ({ message_id, role: "user", text: "work", event_hash: `event-${message_id}`, created_at_ms: 1 })
  const deferred = () => {
    let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
    const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
    return { promise, resolve, reject }
  }
  let conversationHold: ReturnType<typeof deferred> | null = null, messageHold: ReturnType<typeof deferred> | null = null
  let uploadHold: ReturnType<typeof deferred> | null = null
  const acknowledgements: string[] = []
  let unreadNotifications: { notification_id: string; kind: string; text: string; conversation_id: string; target?: InputTarget }[] = []
  const messages: Record<string, unknown>[] = [], controls: Record<string, unknown>[] = [], creations: Record<string, unknown>[] = []
  let failControl = false, failBotCheck = false, failMessage = false, inputQuerySafe = false
  let inputControlEvent = 'c'.repeat(64), inputQueryStage: InputReceipt['stage'] | null = null
  const inputReleases: Record<string, unknown>[] = []
  const inputReceiptOverrides = new Map<string, InputReceipt>(), inputReceiptQueries: Record<string, unknown>[] = []
  let inputReleaseReply: ((body: Record<string, unknown>) => unknown) | null = null
  const questionRow = { question_id: "q1", conversation_id: "chat" as string | null, bot: { bot_id: "bot", name: "Bot" } as { bot_id: string; name: string } | null, severity: "attention", text: "要不要继续", choices: [{ label: "继续" }, { label: "停下" }], about: { node: "worker", agent: "codex", session_id: "thread", cwd: "/project", summary: "改登录" }, author: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", created_at_ms: 1, answer: null }
  const otherRow = { ...questionRow, question_id: "s1", conversation_id: "other" }
  let questionRows: Record<string, unknown>[] = [questionRow, otherRow]
  const answers: Record<string, unknown>[] = []
  let answerOutcome: "answered" | "already_answered" | "lost" = "lost"
  let createMode: "unknown" | "rejected" | "success" = "unknown"
  let knownBots: ReturnType<typeof parseBot>[] = [bot]
  let knownNodes: ReturnType<typeof parseNodes> = []
  let push: Parameters<ReadyConnection["subscribe"]>[0] = () => {}
  let baseGeneration = 1
  const connection: ReadyConnection = {
    get generation() { return baseGeneration }, entry: { kind: "gateway", device_id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", address: "https://node.example" },
    principal: { network_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", epoch: 1, can_manage: true, source: "membership" },
    info: { device_id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", name: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", version: "test", initialized: true, network_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
    isCurrent: () => activeConnection === connection, subscribe: (listener) => { push = listener; return () => {} }, request: async (method: string) => { if (method === "write_state") return { state: "writable", conflicts: [] }; throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: knownNodes }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: unreadNotifications }
      if (method === "questions") return { questions: questionRows }
      if (method === "open_works") return { works: [] }
      if (method === "workspace_overview") return {}
      if (method === "workspace_state") return { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } }
      if (method === "input_receipt") {
        inputReceiptQueries.push(JSON.parse(JSON.stringify(body)))
        return inputReceiptOverrides.get(String(body.message_id)) ?? { message_id: body.message_id, target: body.target, stage: inputQueryStage ?? (inputQuerySafe ? "refused" : transcript.some((message) => message.message_id === body.message_id) ? "entry_accepted" : "unknown"), actual_turn_id: null, reason: "原输入待确认", retry_safe: inputQuerySafe, control_event: inputQuerySafe || inputQueryStage ? inputControlEvent : null }
      }
      if (method === 'workspace_command') {
        const command = workspaceModel.parseWorkspaceCommand(body.command)
        assert.ok(command.kind === 'control_input' && command.action.kind === 'release')
        assert.equal(command.expected_event, inputControlEvent)
        assert.ok(messages.some((message) => message.message_id === command.message_id), 'Release refers to the original stored input')
        inputReleases.push(JSON.parse(JSON.stringify(body)))
        if (inputReleaseReply) return inputReleaseReply(body)
        throw new Error('release acknowledgement lost')
      }
      if (method === 'upload_attachment') return uploadHold ? uploadHold.promise : { complete: true }
      if (method === "answer") {
        answers.push(JSON.parse(JSON.stringify(body)))
        if (answerOutcome === "lost") throw new Error("connection lost")
        questionRows = questionRows.filter((row) => row.question_id !== body.question_id)
        return { status: answerOutcome, message: { message_id: `a-${body.question_id}`, role: "user", text: answerOutcome === "answered" ? body.text : "停下", answers: body.question_id } }
      }
      if (method === "ack") { acknowledgements.push(String(body.notification_id)); unreadNotifications = unreadNotifications.filter(item => item.notification_id !== body.notification_id); return {} }
      if (method === "conversation") return conversationHold ? conversationHold.promise : { ...context, local_node: fixtureEntry, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, messages: conversationMessages?.[String(body.conversation_id)] ?? transcript, works: [work] }
      if (method === "create_bot") {
        creations.push(JSON.parse(JSON.stringify(body)))
        assert.deepEqual(botBody(JSON.parse(stored.get("vibeface.pending-bot.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") ?? "null")), creations.at(-1))
        if (createMode === "unknown") throw new Error("connection lost")
        if (createMode === "rejected") throw Object.assign(new Error("project unavailable"), { responded: true })
        const created = parseBot({ ...body, models: {}, skills: null, event_hash: "new-bot-event" })
        knownBots = [...knownBots.filter((item) => item.bot_id !== created.bot_id), created]
        return { bot: created }
      }
      if (["interrupt_work", "resolve"].includes(method)) {
        controls.push(JSON.parse(JSON.stringify({ method, ...body })))
        if (failControl) throw new Error("connection lost")
        return { accepted: true }
      }
      assert.equal(method, "message")
      messages.push(JSON.parse(JSON.stringify(body)))
      if (messageHold) return messageHold.promise
      if (failMessage) throw new Error("connection lost")
      transcript = [...transcript.filter((item) => item.message_id !== body.message_id), posted(body.message_id)]
      return { accepted: true, state: "queued", message_id: body.message_id }
    },
  }
  let activeConnection = connection
  const render = () => {
    hooks.reset()
    const state = useWorkspaceState(platform, activeConnection)
    afterRender.splice(0).forEach((run) => run())
    return state
  }
  const reload = async () => {
    hooks.remount()
    render(); render().setView("chat"); render()
    // The outbox read that unlocks writing is asynchronous; the next render starts the first refresh.
    await drainOutbox(); render()
    await drainOutbox()
  }
  try {
    render(); render().setView("chat"); render()
    // The outbox read that unlocks writing is asynchronous; the next render starts the first refresh.
    await drainOutbox(); render()
    await drainOutbox()
    let state = render()
    assert.equal(state.writable, true)
    await state.refresh()
    state = render()
    assert.equal(state.selectedBot?.bot_id, "bot")
    await t.test("manual node checks wait for a polling read and then run one live probe", async () => {
      const rpc = connection.rpc
      const polling = deferred()
      const paramsSeen: unknown[] = []
      connection.rpc = (method, params) => {
        if (method !== "nodes") return rpc(method, params)
        paramsSeen.push(params)
        return paramsSeen.length === 1 ? polling.promise : Promise.resolve({ nodes: [] })
      }
      try {
        const first = state.refreshNodes()
        const manual = state.refreshNodes(true)
        const repeated = state.refreshNodes(true)
        assert.deepEqual(paramsSeen, [undefined])
        polling.resolve({ nodes: [] })
        await Promise.all([first, manual, repeated])
        assert.equal(paramsSeen.length, 2)
        assert.equal(workspaceModel.record(paramsSeen[1]).check_connections, true)
      } finally {
        connection.rpc = rpc
        polling.resolve({ nodes: [] })
        await state.refreshNodes()
      }
    })
    await t.test('malformed signed Unknown inputs survive reload and recovery attempts without rewriting or resubmission', async () => {
      const originalOutbox = platform.outbox
      const mutations: string[] = []
      platform.outbox = { all: originalOutbox.all, put: async (item) => { mutations.push('put'); await originalOutbox.put(item) }, delete: async (key) => { mutations.push('delete'); await originalOutbox.delete(key) } }
      try {
        for (const report_results of [undefined, null, 'false', 0]) {
          const invalid = { key: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:message:legacy', method: 'message', created_at_ms: 1, delivery: 'unknown',
            params: { conversation_id: 'chat', message_id: 'legacy', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, text: '原签名请求', ...(report_results === undefined ? {} : { report_results }), attestation: { device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', signature: 'must-stay-original' } },
            uploads: [{ hash: 'a'.repeat(64), size: 3, name: 'original.txt', data: 'YWJj' }],
          }
          corruptOutbox.set(invalid.key, invalid)
          const bytes = JSON.stringify([...corruptOutbox]), signatures = signed.length, submissions = messages.length, queries = inputReceiptQueries.length
          mutations.length = 0
          await reload()
          assert.match(render().error?.text ?? '', /损坏/)
          assert.equal(render().canSaveMessages, false)
          await render().refresh()
          await drainOutbox()
          await assert.rejects(render().retryPendingMessage('legacy'), /损坏/)
          await assert.rejects(render().releaseMessagePending('legacy'), /损坏/)
          assert.equal(JSON.stringify([...corruptOutbox]), bytes, 'raw ID, signature, delivery and upload bytes survive every failed boundary')
          assert.deepEqual(mutations, [], 'invalid durable wire is neither pruned nor rewritten')
          assert.deepEqual([signed.length, messages.length, inputReceiptQueries.length], [signatures, submissions, queries], 'no signing, write or input reconciliation is attempted')
          await originalOutbox.delete(invalid.key)
        }
      } finally {
        platform.outbox = originalOutbox
        await originalOutbox.delete('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:message:legacy')
        await reload()
        await render().refresh()
      }
    })
    await t.test('a lost first submission reconciles only at its bound entry and retries the original signed input by choice', async () => {
      const previousOutbox = new Map(outboxItems), previousTranscript = transcript
      const originalRpc = connection.rpc, entry = connection.entry.device_id, node = connection.info.device_id
      const firstMessage = messages.length, firstSignature = signed.length
      const queries: Record<string, unknown>[] = []
      let missing = false, negativeOverride: Record<string, unknown> = {}
      connection.rpc = async (method, body = {}) => {
        if (method !== 'input_receipt') return originalRpc(method, body)
        queries.push(structuredClone(body))
        if (missing && body.original_input !== undefined) return { accepted: false, code: 'input_not_saved', stored: false, entry_device_id: entry, network_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', client_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', conversation_id: body.conversation_id, message_id: body.message_id, target: body.target, reason: '原入口未保存这条输入。', ...negativeOverride }
        throw Object.assign(new Error(missing ? 'input not found' : 'receipt query unavailable'), { responded: missing })
      }
      try {
        failMessage = true
        assert.equal(await render().sendMessage('首次输入未落库'), true)
        await drainOutbox()
        const head = render().pendingMessages.find((item) => item.text === '首次输入未落库')
        assert.ok(head)
        const original = messages[firstMessage]
        assert.ok(original)
        const item = [...outboxItems.values()].find((item): item is OutboxRecord<'message'> => isMessageOutbox(item) && item.params.message_id === head.message_id)
        assert.ok(item)
        assert.equal(item.entry_device_id, entry, 'the first call must already have a durable original entry binding')
        assert.equal(item.delivery, 'unknown')
        assert.equal(await render().sendMessage('同目标后续输入'), true)
        assert.equal(await render().sendMessage('首次输入未落库'), true)
        await drainOutbox()
        assert.equal(messages.length, firstMessage + 1, 'the unknown first write holds later input')
        const duplicate = render().pendingMessages.find((item) => item.message_id !== head.message_id && item.text === head.text)
        assert.ok(duplicate)
        assert.equal(duplicate.delivery, 'saved')
        await render().releaseMessagePending(duplicate.message_id)
        assert.equal(messages.length, firstMessage + 1, 'discarding a later unsent duplicate never sends the first or later input')
        assert.equal(render().pendingMessages.find((item) => item.message_id === head.message_id)?.delivery, 'unknown')

        missing = true
        connection.entry.device_id = connection.info.device_id = 'd'.repeat(64)
        await assert.rejects(render().queryInput(head), /input not found/)
        assert.equal(queries.at(-1)?.original_input, undefined, 'another Node only provides positive reconciliation')
        assert.equal(render().pendingMessages.find((item) => item.message_id === head.message_id)?.delivery, 'unknown')
        connection.entry.device_id = entry; connection.info.device_id = node
        for (const changed of [{ client_id: 'another-client' }, { target: { kind: 'bot_main', bot_id: 'another-bot' } }]) {
          negativeOverride = changed
          await assert.rejects(render().queryInput(head), /不一致/)
          assert.equal(render().pendingMessages.find((item) => item.message_id === head.message_id)?.delivery, 'unknown')
        }
        negativeOverride = {}
        await render().queryInput(head)
        const recovered = render().pendingMessages.find((item) => item.message_id === head.message_id)
        assert.equal(recovered?.delivery, 'saved')
        assert.equal(recovered?.rejection_code, 'input_not_saved')
        assert.deepEqual(queries.at(-1)?.original_input, original)
        assert.equal(messages.length, firstMessage + 1, 'a proven absence never automatically replays input')
        const boundRefusal = (await memoryOutbox.all()).find((pending) => pending.key === item.key)
        assert.ok(boundRefusal)
        await memoryOutbox.put({ ...boundRefusal, rejected: undefined })
        await reload(); await render().refresh(); await drainOutbox()
        assert.equal(messages.length, firstMessage + 1, 'a bound negative receipt still waits for explicit retry when its error text is missing')
        assert.deepEqual((await memoryOutbox.all()).find((pending) => pending.key === item.key), { ...boundRefusal, rejected: undefined })
        failMessage = false
        connection.entry.device_id = connection.info.device_id = 'd'.repeat(64)
        await assert.rejects(render().retryPendingMessage(head.message_id), /原设备/)
        assert.equal(messages.length, firstMessage + 1)
        connection.entry.device_id = entry; connection.info.device_id = node
        await render().retryPendingMessage(head.message_id)
        await drainOutbox()
        assert.deepEqual(messages[firstMessage + 1], original, 'retry sends the exact original signed request')
        assert.equal(messages.length, firstMessage + 3, 'the later saved input advances after the original retry is accepted')
        assert.equal(signed.length, firstSignature + 3, 'reconciliation, discard and retry do not sign another input')

        failMessage = true
        await render().sendMessage('稍后才落库的原输入'); await drainOutbox()
        const late = render().pendingMessages.find((item) => item.text === '稍后才落库的原输入')
        assert.ok(late)
        await render().queryInput(late)
        assert.equal(render().pendingMessages.find((item) => item.message_id === late.message_id)?.rejection_code, 'input_not_saved')
        const writes = messages.length
        transcript = [...transcript, posted(late.message_id)]
        connection.rpc = originalRpc
        await render().queryInput(late)
        const received = render().pendingMessages.find((item) => item.message_id === late.message_id)
        assert.ok(!received || received.delivery === 'accepted' && received.rejected === undefined, 'a late positive receipt supersedes the earlier absence proof')
        assert.equal(messages.length, writes, 'reconciling a late positive receipt performs no message write')
      } finally {
        failMessage = false
        connection.entry.device_id = entry; connection.info.device_id = node
        connection.rpc = originalRpc
        messages.splice(firstMessage); signed.splice(firstSignature)
        transcript = previousTranscript
        outboxItems.clear()
        for (const [key, item] of previousOutbox) outboxItems.set(key, item)
        await reload(); await render().refresh()
      }
    })
    await t.test('missing delivery or original entry pauses the entire final-schema ledger without guessing', async () => {
      const previousOutbox = new Map(outboxItems), submissions = messages.length, signatures = signed.length
      try {
        for (const missing of ['delivery', 'entry_device_id'] as const) {
          const key = `${fixtureNetwork}.${fixtureClient}:message:missing-final-field`
          const value: Record<string, unknown> = signedItem({ key, method: 'message', scope_id: 'chat', created_at_ms: 1, delivery: 'unknown', entry_device_id: fixtureEntry, params: { conversation_id: 'chat', message_id: 'missing-final-field', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, text: '原输入', report_results: true, attestation: { device_id: fixtureClient, signature: '0'.repeat(128) } } })
          delete value[missing]
          outboxItems.clear(); corruptOutbox.set(key, value)
          const bytes = JSON.stringify([...corruptOutbox])
          await reload(); await render().refresh()
          assert.equal(render().canSaveMessages, false)
          for (const action of [() => render().confirmInputEntry('missing-final-field', fixtureEntry), () => render().retryPendingMessage('missing-final-field'), () => render().releaseMessagePending('missing-final-field')]) await assert.rejects(action, /损坏/)
          assert.equal(JSON.stringify([...corruptOutbox]), bytes)
          assert.deepEqual([messages.length, signed.length], [submissions, signatures])
          corruptOutbox.clear()
        }
      } finally { corruptOutbox.clear(); outboxItems.clear(); for (const [key, item] of previousOutbox) outboxItems.set(key, item); await reload(); await render().refresh() }
    })
    await t.test('an old absence write cannot replace positive receipts or restore a history-admitted input', async (race) => {
      const previousOutbox = new Map(outboxItems), previousTranscript = transcript
      const originalRpc = connection.rpc, originalOutbox = platform.outbox, originalLock = platform.inputLock
      const submissions = messages.length, signatures = signed.length
      platform.inputLock = (_name, save) => Promise.resolve(save())
      try {
        for (const result of ['entry_accepted', 'unknown', 'history', 'workspace_history'] as const) await race.test(result, async () => {
          outboxItems.clear(); transcript = []
          const item: OutboxRecord<'message'> = { scope_id: 'chat', key: `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:message:race-${result}`, method: 'message', created_at_ms: 1, delivery: 'unknown', entry_device_id: 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', rejected: '等待查询',
            params: { conversation_id: 'chat', message_id: `race-${result}`, target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, text: '原输入', report_results: true, attestation: { device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', signature: 'original-signature' } } }
          item.params.attestation = signatureFor(item.scope_id, writeBody(item, fixtureClient))
          await memoryOutbox.put(item)
          await reload(); await render().refresh()
          const input = workspaceModel.parsePendingMessage(item), writing = deferred(), resume = deferred()
          const absence = { accepted: false, code: 'input_not_saved', stored: false, entry_device_id: 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', network_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', client_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', conversation_id: 'chat', message_id: input.message_id, target: input.target, reason: '原设备未保存这条输入。' }
          const receipt: InputReceipt = { message_id: input.message_id, target: input.target, stage: result === 'unknown' ? 'unknown' : 'entry_accepted', actual_turn_id: null, reason: '原输入已有记录', retry_safe: false, control_event: null }
          let firstQuery = true, holdWrite = true
          connection.rpc = async (method, body = {}) => {
            if (method === 'workspace_history') return { history_authority: 'authority', next_cursor: null, complete: true, events: [{ event_hash: 'input-event', created_at_ms: 1, scope_id: 'chat', kind: 'input_posted', speaker: { kind: 'human', device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' }, body: { ...item.params, type: 'input_posted' } }], coverage: { limit: 128, truncated: false, scope: 'workspace' } }
            if (method !== 'input_receipt') return originalRpc(method, body)
            if (firstQuery) { firstQuery = false; return absence }
            return receipt
          }
          platform.outbox = { ...originalOutbox, put: async (next) => {
            if (holdWrite && isMessageOutbox(next) && next.key === item.key && next.rejection_code === 'input_not_saved') {
              holdWrite = false; writing.resolve(undefined); await resume.promise
            }
            await originalOutbox.put(next)
          } }
          const negative = render().queryInput(input)
          let positive = Promise.resolve<unknown>(undefined)
          try {
            await writing.promise
            if (result === 'history') { transcript = [posted(input.message_id)]; positive = render().refresh() }
            else if (result === 'workspace_history') positive = render().readWorkspaceHistory()
            else positive = render().queryInput(input)
            await drainOutbox()
            resume.resolve(undefined)
            await Promise.all([negative, positive])
            const latest = (await memoryOutbox.all()).find((entry): entry is OutboxRecord<'message'> => isMessageOutbox(entry) && entry.key === item.key)
            if (result === 'history' || result === 'workspace_history') assert.equal(latest, undefined, 'history admission cannot be undone by an earlier absence write')
            else {
              assert.ok(latest)
              assert.equal(latest.delivery, result === 'unknown' ? 'unknown' : 'accepted')
              assert.equal(latest.receipt?.stage, result)
              assert.equal(latest.rejection_code, undefined)
              assert.equal(latest.rejected, undefined)
              assert.deepEqual(latest.params, item.params)
              if (result === 'unknown') {
                firstQuery = true
                assert.deepEqual(await render().queryInput(input), receipt, 'retained input knowledge outranks a later absence response')
                assert.equal((await memoryOutbox.all()).find((entry): entry is OutboxRecord<'message'> => isMessageOutbox(entry) && entry.key === item.key)?.receipt?.stage, 'unknown')
              }
            }
          } finally { resume.resolve(undefined); await Promise.allSettled([negative, positive]); platform.outbox = originalOutbox }
        })
        assert.equal(messages.length, submissions)
        assert.equal(signed.length, signatures)
      } finally {
        platform.outbox = originalOutbox; platform.inputLock = originalLock; connection.rpc = originalRpc
        transcript = previousTranscript; outboxItems.clear()
        for (const [key, item] of previousOutbox) outboxItems.set(key, item)
        await reload(); await render().refresh()
      }
    })
    await t.test('an unbound saved refusal remains raw evidence and cannot be discarded or sent', async () => {
      const previousOutbox = new Map(outboxItems), submissions = messages.length, signatures = signed.length
      const key = `${fixtureNetwork}.${fixtureClient}:message:unbound-refusal`
      const item = signedItem({ key, method: 'message', scope_id: 'chat', created_at_ms: 1, delivery: 'saved', rejected: '', rejection_code: 'input_not_saved', params: { conversation_id: 'chat', message_id: 'unbound-refusal', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, text: '原输入', report_results: true, attestation: { device_id: fixtureClient, signature: '0'.repeat(128) } } })
      try {
        outboxItems.clear(); corruptOutbox.set(key, item); const bytes = JSON.stringify([...corruptOutbox])
        await reload(); await render().refresh()
        for (const action of [() => render().confirmInputEntry('unbound-refusal', fixtureEntry), () => render().retryPendingMessage('unbound-refusal'), () => render().releaseMessagePending('unbound-refusal'), () => render().sendMessage('later')]) await assert.rejects(action, /损坏/)
        assert.equal(render().canSaveMessages, false); assert.equal(JSON.stringify([...corruptOutbox]), bytes)
        assert.deepEqual([messages.length, signed.length], [submissions, signatures])
      } finally { corruptOutbox.clear(); outboxItems.clear(); for (const [key, item] of previousOutbox) outboxItems.set(key, item); await reload(); await render().refresh() }
    })
    await t.test('shared public history API fences late results, preserves pushed rows and restarts after authority changes', async () => {
      const originalRpc = connection.rpc
      const row = (message_id: string, at: number, text = message_id) => ({ message_id, event_hash: message_id, rank: String(at).padStart(20, '0'), role: 'user', text, created_at_ms: at })
      let authority = 'authority', reads = 0
      let newest = row('latest', 10)
      let held: ReturnType<typeof deferred> | null = null
      const calls: Record<string, unknown>[] = []
      connection.rpc = async (method, params = {}) => {
        if (method === 'conversation') return { ...context, history_authority: authority, messages: [newest], history: { ...completeHistory(), messages: { next_cursor: 'first', complete: false }, works: { next_cursor: 'works-first', complete: false } } }
        if (method !== 'conversation_history') return originalRpc(method, params)
        calls.push(params)
        if (params.section === 'works') return { conversation_id: 'chat', section: 'works', history_authority: authority, items: [{ ...work, work_id: 'older-work', state: 'running' }], next_cursor: null, complete: true }
        if (held) return held.promise
        reads++
        return { conversation_id: 'chat', section: 'messages', history_authority: authority, items: reads === 1 ? [] : [row('old', 1), row('latest', 10, 'stale')], next_cursor: reads === 1 ? 'second' : 'third', complete: false }
      }
      try {
        await render().refresh()
        await render().loadHistory('messages')
        assert.deepEqual(calls.map((call) => call.cursor), ['first', 'second'])
        assert.deepEqual(Array.from(render().data?.messages ?? [], (item) => item.message_id), ['old', 'latest'])
        await render().refresh()
        assert.deepEqual(Array.from(render().data?.messages ?? [], (item) => item.message_id), ['old', 'latest'])
        assert.equal(render().data?.history.messages.next_cursor, 'third')
        held = deferred()
        const previousWindow = render().loadHistory('messages')
        newest = row('disjoint-newest', 20)
        await render().refresh()
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('wrong-window', 0)], next_cursor: null, complete: true })
        await previousWindow
        assert.equal(render().data?.messages.some((item) => item.message_id === 'wrong-window'), false)
        assert.equal(render().data?.history.messages.next_cursor, 'first', 'a late page cannot hide the newly exposed refresh gap')
        newest = row('latest', 10)
        await render().refresh()

        held = deferred()
        const pending = render().loadHistory('messages')
        assert.equal(render().historyLoading.has('messages'), true)
        render().cancelHistory('messages')
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('cancelled', 0)], next_cursor: null, complete: true })
        await pending
        assert.equal(render().data?.messages.some((item) => item.message_id === 'cancelled'), false)
        held = deferred()
        const paging = render().loadHistory('messages')
        push(parseChanged({ type: 'records', conversation_id: 'chat', history_authority: authority, refresh_metadata: true, messages: [row('latest', 10, 'pushed')], works: [] }))
        await drainOutbox()
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('latest', 10, 'stale')], next_cursor: 'fourth', complete: false })
        await paging
        assert.equal(render().data?.messages.find((item) => item.message_id === 'latest')?.text, 'pushed')
        await render().refresh()
        held = deferred()
        const invalidated = render().loadHistory('messages')
        authority = 'fork-authority'
        held.reject(new Error('conversation owner changed; refresh the conversation'))
        await invalidated
        assert.equal(render().data?.history_authority, 'fork-authority')
        assert.deepEqual(Array.from(render().data?.messages ?? [], (item) => item.message_id), ['latest'])
        held = deferred()
        const previousVisit = render().loadHistory('messages')
        await render().selectBot({ ...bot, conversation_id: 'another-chat' })
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('wrong-visit', 0)], next_cursor: null, complete: true })
        await previousVisit
        assert.equal(render().data?.messages.some((item) => item.message_id === 'wrong-visit'), false)
        held = null
        await render().selectBot(bot)
        await render().refresh()
        await render().loadHistory('works')
        assert.equal(render().data?.works.find((item) => item.work_id === 'older-work')?.state, 'running')
        held = deferred()
        const oldGeneration = render().loadHistory('messages')
        baseGeneration += 1
        render()
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('wrong-generation', 0)], next_cursor: null, complete: true })
        await oldGeneration
        await drainOutbox(); render()
        await drainOutbox()
        await render().refresh()
        assert.equal(render().data?.history_authority, 'fork-authority', 'same-authority reconnect still resets retained history')
        assert.equal(render().data?.works.some((item) => item.work_id === 'older-work'), false, 'a missed terminal Work push outside the latest window cannot remain visibly running')
        assert.equal(render().data?.messages.some((item) => item.message_id === 'wrong-generation'), false)

        held = deferred()
        const oldNetwork = render().loadHistory('messages')
        const nextConnection: ReadyConnection = { ...connection, generation: 2, principal: { ...connection.principal, network_id: 'e'.repeat(64) }, isCurrent: () => activeConnection === nextConnection }
        activeConnection = nextConnection
        render()
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('wrong-network', 0)], next_cursor: null, complete: true })
        await oldNetwork
        assert.equal(render().data, null)
        activeConnection = connection
        render()
        await drainOutbox()
        render()

      } finally {
        activeConnection = connection
        baseGeneration = 1
        render()
        connection.rpc = originalRpc
        await render().selectBot(bot)
        await render().refresh()
      }
    })
    await t.test('history authority invalidation cancels an in-flight Alpha snapshot until a queued Beta workspace refresh confirms it', async (race) => {
      for (const cacheEnabled of [false, true]) await race.test(cacheEnabled ? 'cached mobile view' : 'default Web view', async () => {
        const originalRpc = connection.rpc, originalTimeout = sandbox.setTimeout, originalClearTimeout = sandbox.clearTimeout
        const originalCache = platform.conversationCache
        const timers = new Map<number, () => void>()
        let timerId = 0, phase: 'confirmed' | 'alpha-refresh' | 'beta-refresh' = 'confirmed'
        const alphaStarted = deferred(), oldAlpha = deferred(), betaStarted = deferred(), beta = deferred()
        const row = (message_id: string, rank: string) => ({ message_id, event_hash: message_id, rank, role: 'user', text: message_id, created_at_ms: 1 })
        const snapshot = (authority: string, message_id: string) => ({ ...context, history_authority: authority, messages: [row(message_id, '2')], history: { ...completeHistory(), messages: { next_cursor: `${authority}-cursor`, complete: false } } })
        const confirmedAlpha = snapshot('alpha', 'confirmed-alpha'), lateAlpha = snapshot('alpha', 'late-alpha'), confirmedBeta = snapshot('beta', 'confirmed-beta')
        const pages: Record<string, unknown>[] = []
        let refreshing: Promise<void> | undefined, invalidated: Promise<void> | undefined
        platform.conversationCache = cacheEnabled
        sandbox.setTimeout = (run: () => void) => { const id = ++timerId; timers.set(id, run); return id }
        sandbox.clearTimeout = (id: number) => { timers.delete(id) }
        connection.rpc = async (method, params = {}) => {
          if (method === 'conversation') {
            if (phase === 'alpha-refresh') { alphaStarted.resolve(undefined); return oldAlpha.promise }
            if (phase === 'beta-refresh') { betaStarted.resolve(undefined); return beta.promise }
            return confirmedAlpha
          }
          if (method === 'conversation_history') {
            pages.push(params)
            if (phase === 'alpha-refresh') throw new Error('history changed; refresh the conversation')
            return { conversation_id: 'chat', section: 'messages', history_authority: 'beta', items: [row('older-beta', '1')], next_cursor: null, complete: true }
          }
          return originalRpc(method, params)
        }
        try {
          await reload(); await render().refresh()
          const alpha = render()
          assert.equal(alpha.dataOrigin, 'network')
          assert.equal(alpha.data?.history_authority, 'alpha')
          assert.equal(alpha.writable, true)
          phase = 'alpha-refresh'
          refreshing = alpha.refresh()
          await alphaStarted.promise
          invalidated = render().loadHistory('messages')
          await settleComponent()
          assert.equal(pages[0]?.cursor, 'alpha-cursor')
          assert.equal(render().dataOrigin, cacheEnabled ? 'cache' : 'none')
          assert.equal(render().writable, false, 'an invalidated cursor pauses writes even while older workspace metadata is being refreshed')
          phase = 'beta-refresh'
          oldAlpha.resolve(lateAlpha)
          await Promise.all([refreshing, invalidated])
          const stale = render()
          assert.equal(stale.dataOrigin, cacheEnabled ? 'cache' : 'none', 'the late Alpha request cannot reapply a network origin after its read owner is invalidated')
          assert.deepEqual(Array.from(stale.data?.messages ?? [], message => message.message_id), cacheEnabled ? ['confirmed-alpha'] : [])
          assert.equal(stale.writable, false, 'the old full refresh cannot grant writes using its cancelled conversation read')
          assert.equal(stale.synced, false)
          await assert.rejects(stale.loadHistory('messages'), /先选择并读取对话/, 'an unconfirmed Alpha view cannot authorize another page')
          assert.equal(timers.size, 1, 'the complete Beta refresh is queued separately from the stale Alpha request')
          const queued = [...timers.values()]; timers.clear(); queued.forEach(run => run())
          await betaStarted.promise
          assert.equal(render().writable, false, 'queued metadata alone cannot authorize writes before Beta returns')
          assert.equal(render().dataOrigin, cacheEnabled ? 'cache' : 'none')
          beta.resolve(confirmedBeta)
          await settleComponent()
          const recovered = render()
          assert.equal(recovered.dataOrigin, 'network')
          assert.equal(recovered.data?.history_authority, 'beta')
          assert.equal(recovered.writable, true)
          assert.equal(recovered.synced, true)
          await recovered.loadHistory('messages')
          assert.equal(pages.at(-1)?.cursor, 'beta-cursor')
          assert.deepEqual(Array.from(render().data?.messages ?? [], message => message.message_id), ['older-beta', 'confirmed-beta'])
          assert.equal(render().data?.history.messages.complete, true)
        } finally {
          oldAlpha.resolve(lateAlpha); beta.resolve(confirmedBeta)
          await Promise.allSettled([refreshing, invalidated])
          timers.clear()
          sandbox.setTimeout = originalTimeout; sandbox.clearTimeout = originalClearTimeout
          connection.rpc = originalRpc; platform.conversationCache = originalCache
          await reload(); await render().refresh()
        }
      })
    })
    const retryMessage = { message_id: "retry-1", event_hash: "retry-event", role: "user", text: "改 README", created_at_ms: 1 }
    const retryScheduler = { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: "message:retry-1", state: "failed", message_id: "retry-1", failure: { kind: "rejected", detail: "没有执行" } }], unanswered: [] }
    transcript = [retryMessage]
    scheduler = retryScheduler
    const heldRefresh = deferred()
    conversationHold = heldRefresh
    const refreshing = render().refresh()
    await drainOutbox()
    const retrying = render().retryReply("retry-1")
    await drainOutbox()
    assert.equal(messages.length, 0, "retry waits for an in-flight full read before sending")
    heldRefresh.resolve({ ...context, messages: [retryMessage], scheduler: retryScheduler })
    await refreshing
    assert.equal(await retrying, true)
    await drainOutbox()
    assert.equal(messages.length, 1)
    assert.match(String(messages[0]?.text), /上次没回完/)
    conversationHold = null
    await render().refresh()
    messages.splice(0)
    conversationHold = null
    scheduler = null
    transcript = []
    stored.set("vibeface.ignored-inbox.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", JSON.stringify([""]))
    unreadNotifications = [{ notification_id: "local-note", kind: "manager_attention", text: "看看进展", conversation_id: "chat", target: { kind: "bot_main", bot_id: "bot" } }]
    render().setView("tasks")
    await render().refresh()
    render().ignoreNotifications(["local-note"])
    assert.deepEqual(JSON.parse(stored.get("vibeface.ignored-inbox.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") ?? "null"), ["", "local-note"], "ignoring merges the latest local storage from another tab")
    unreadNotifications = []
    render().setView("chat")
    failMessage = true
    assert.equal(await render().sendMessage("work", [{ name: "retry.txt", arrayBuffer: async () => new Uint8Array([7, 8, 9]).buffer }]), true)
    await drainOutbox()
    state = render()
    const saved = state.pendingMessages.find((item) => item.text === "work")
    assert.ok(saved)
    const pending = JSON.parse(JSON.stringify(saved))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, target: { kind: "bot_main", bot_id: "bot" }, intent: "continue", sequence: pending.sequence, text: "work", attachments: pending.attachments, report_results: true, attestation: signatureFor("chat", { type: "input_posted", message_id: pending.message_id, target: pending.target, intent: pending.intent, sequence: pending.sequence, text: "work", attachments: pending.attachments, report_results: true }) })
    assert.equal(signed.at(-1), `attestation|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|chat|${JSON.stringify({ type: "input_posted", message_id: pending.message_id, target: pending.target, intent: pending.intent, sequence: pending.sequence, text: "work", attachments: pending.attachments, report_results: true })}`)
    assert.equal(render().canSaveMessages, true, "an unknown prior input leaves local saving available")
    failMessage = false
    connection.entry.device_id = "other-gateway"
    const inputUiErrors: string[] = []
    let inputWork = Promise.resolve()
    const inputUi = componentHarness('./workspace-chat.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => render() }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, action: () => Promise<void>) => { inputWork = action().catch((cause) => { inputUiErrors.push(String(cause)) }); return inputWork } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
    })
    const inputButton = (label: string) => {
      const saved = render().pendingMessages.find((item) => item.message_id === pending.message_id)
      assert.ok(saved)
      return inputUi.render('WorkspaceInputStatus', { pending: saved }).find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props
    }
    const pressInput = async (label: string) => { const press = inputButton(label)?.onClick; assert.ok(press); press(); await inputWork; await settleComponent() }
    try {
      await pressInput('查询结果')
      assert.equal(inputButton('重试原输入'), undefined, 'unknown writes are queried without replay')
      assert.equal(messages.length, 1)
      inputQuerySafe = true
      await pressInput('查询结果')
      transcript = [{ ...posted(pending.message_id), target: pending.target, attachments: pending.attachments }]
      await render().refresh()
      assert.ok(inputButton('重试原输入'), 'a stored safe refusal survives history until the user chooses recovery')
      await pressInput('重试原输入')
      assert.equal(inputReleases.length, 1, inputUiErrors.join(' | '))
      assert.equal(messages.length, 1, 'the recovery is a Release CAS, never a second message write')
      assert.equal(inputButton('重试原输入'), undefined, 'a lost Release answer must not offer another Release')
      await pressInput('查询结果')
      const unresolved = render().pendingMessages.find((item) => item.message_id === pending.message_id)
      assert.equal(unresolved?.delivery, 'unknown', 'an old refused receipt cannot confirm the lost Release')
      const retained = [...outboxItems.values()].find((item): item is OutboxRecord<'message'> => isMessageOutbox(item) && item.params.message_id === pending.message_id)
      assert.ok(retained)
      assert.deepEqual(retained.params, messages[0], 'original ID, signature, sequence and attachments stay unchanged')
      assert.equal(signed.filter((value) => value.includes('"type":"input_posted"') && value.includes(pending.message_id)).length, 1)
      assert.equal(inputReleases.length, 1)
      const retainedControl = [...outboxItems.values()].find(isWorkspaceCommandOutbox)
      assert.ok(retainedControl); assert.deepEqual(retainedControl.params, inputReleases[0], 'the unknown Release retains its original signed control request')
      inputControlEvent = 'd'.repeat(64); inputQuerySafe = false; inputQueryStage = 'entry_accepted'
      await pressInput('查询结果')
      const settled = render().pendingMessages.find((item) => item.message_id === pending.message_id)
      assert.ok(!settled || settled.delivery === 'accepted')
      if (settled) assert.equal(inputButton('重试原输入'), undefined)
    } finally { inputUi.close(); inputQuerySafe = false; inputQueryStage = null }
    connection.entry.device_id = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    await render().refresh()
    assert.equal(render().pendingMessages.length, 0)
    await render().interruptWork("work")
    assert.deepEqual(controls, [{ method: "interrupt_work", conversation_id: "chat", work_id: "work" }])
    const [question, other] = render().questions
    assert.equal(question.text, "要不要继续")
    connection.principal.can_manage = false
    assert.equal(render().writable, true, "an ordinary joined member can answer without manager authority")
    await assert.rejects(render().answer(question, "  "), /不能为空/)
    await assert.rejects(render().answer(question, " 继续 "), /connection lost/)
    assert.deepEqual(JSON.parse(JSON.stringify(render().pendingAnswers)), [{ question_id: "q1", text: "继续" }], "an unknown outcome waits in the outbox")
    assert.equal(signed.at(-1), 'attestation|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|chat|{"type":"message_posted","message_id":"a-q1","role":"user","text":"继续","report_results":true,"answers":"q1"}', "the answer is signed as the chat message the Node writes")
    const signatures = signed.length
    await assert.rejects(render().answer(question, "停下"), /还没确认送达/)
    assert.equal(answers.length, 1, "another answer waits until the first is settled")
    answerOutcome = "answered"
    assert.deepEqual(JSON.parse(JSON.stringify(await render().answer(question, "继续"))), { status: "answered", text: "继续" })
    assert.deepEqual(answers, [{ question_id: "q1", text: "继续", attestation: signatureFor("chat", writeBody({ method: "answer", params: { question_id: "q1", text: "继续" } }, fixtureClient)) }, { question_id: "q1", text: "继续", attestation: signatureFor("chat", writeBody({ method: "answer", params: { question_id: "q1", text: "继续" } }, fixtureClient)) }], "trying again sends the stored request")
    assert.equal(signed.length, signatures, "nothing is signed anew")
    assert.deepEqual(render().pendingAnswers, [])
    assert.equal(render().answered.q1, "继续", "the answer shows before the next read")
    answerOutcome = "already_answered"
    assert.equal((await render().answer(other, "继续")).text, "停下", "the answer that stood first is the one shown")
    assert.equal(signed.at(-1), 'attestation|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|other|{"type":"message_posted","message_id":"a-s1","role":"user","text":"继续","report_results":true,"answers":"s1"}', "an answer is signed for its question's chat")
    const memberPrincipal = connection.principal, beforeLocal = answers.length
    connection.principal = { ...memberPrincipal, source: "local_session", can_manage: false }
    assert.equal(render().writable, false)
    await assert.rejects(render().answer(question, "允许"), /本机管理不能发送/)
    assert.equal(answers.length, beforeLocal, "a local session without membership never writes a new answer")
    connection.principal = memberPrincipal
    connection.principal.can_manage = true
    // After a new connection the outbox asks the questions list before sending an unknown answer again.
    const later = { ...questionRow, question_id: "q2" }, elsewhere = { ...questionRow, question_id: "q3" }
    questionRows = [later, elsewhere]
    await render().refresh()
    answerOutcome = "lost"
    await assert.rejects(render().answer(render().questions[0], "继续"), /connection lost/)
    await assert.rejects(render().answer(render().questions[1], "继续"), /connection lost/)
    questionRows = [later]
    answerOutcome = "answered"
    const beforeSettle = answers.length
    await reload()
    for (let turn = 0; turn < 10; turn++) await drainOutbox()
    assert.deepEqual(answers.slice(beforeSettle).map((item) => item.question_id), ["q2"], "an answer whose question is still open is sent again; one already answered is not")
    assert.deepEqual(render().pendingAnswers, [])
    await render().resolveCheck({ check_id: "work:work", kind: "work", detail: "", work_id: "work", target_node: "worker", conversation_id: "chat", title: "", reason: "offline" }, "completed", " 设备上已看到结果 ")
    assert.deepEqual(controls.pop(), { method: "resolve", conversation_id: "chat", check_id: "work:work", outcome: "completed", note: "设备上已看到结果", attestation: signatureFor("chat", { type: "work_finished", work_id: "work", outcome: "completed", result_text: "设备上已看到结果" }) })
    assert.equal(signed.at(-1), 'attestation|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|chat|{"type":"work_finished","work_id":"work","outcome":"completed","result_text":"设备上已看到结果"}', "the choice is signed as the settlement the Node writes")
    await assert.rejects(render().interruptWork("other-work"), /停不了/)
    const beforeUnknown = controls.length
    failControl = true
    await assert.rejects(render().interruptWork("work"), /connection lost/)
    assert.equal(render().writable, false)
    await assert.rejects(render().interruptWork("work"), /补读/)
    failControl = false
    await render().refresh()
    assert.equal(controls.length, beforeUnknown + 1)
    assert.equal(render().writable, true)
    work.state = "finished"; work.outcome = "completed"
    await render().refresh()
    await assert.rejects(render().interruptWork("work"), /停不了/)
    work.state = "running"; work.outcome = null
    await render().refresh()
    await assert.rejects(render().createBot({ name: "large", instructions: "中".repeat(2731) }), /8192/)
    assert.equal(render().pendingBot, null)
    await assert.rejects(render().createBot({ name: "n".repeat(512), instructions: "a".repeat(8192), project: { target_node: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", cwd: "/" + "p".repeat(4095) } }), /太长/)
    assert.equal(render().pendingBot, null)
    await assert.rejects(render().createBot({ name: "项目 Bot", instructions: "只处理当前项目", project: { target_node: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", cwd: "/project" } }), /connection lost/)
    const originalCreation = creations[0]
    assert.equal(typeof originalCreation.bot_id, "string")
    assert.notEqual(originalCreation.bot_id, originalCreation.conversation_id)
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    await reload()
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    await assert.rejects(render().createBot({ name: "改名", instructions: "只处理当前项目", project: { target_node: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", cwd: "/project" } }), /内容已保留/)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "项目 Bot", instructions: "只处理当前项目", project: { target_node: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", cwd: "/project" } }), /project unavailable/)
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    assert.equal(render().pendingBot?.submission?.state, "unconfirmed")
    createMode = "success"
    await render().createBot({ name: "项目 Bot", instructions: "只处理当前项目", project: { target_node: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", cwd: "/project" } })
    assert.deepEqual(creations.slice(1), [originalCreation, originalCreation, originalCreation], "the reload resubmits the unconfirmed creation from the outbox")
    assert.equal(render().pendingBot, null)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "invalid project", instructions: "", project: { target_node: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", cwd: "/unavailable" } }), /确认没建成/)
    assert.equal(render().pendingBot, null)
    failBotCheck = true
    await assert.rejects(render().createBot({ name: "rejected with pending lookup", instructions: "" }), /lookup unavailable/)
    assert.equal(render().pendingBot?.submission?.state, "rejected")
    const creationsBeforeCheck = creations.length
    failBotCheck = false
    await reload()
    await assert.rejects(render().createBot({ name: "rejected with pending lookup", instructions: "" }), /确认没建成/)
    assert.equal(creations.length, creationsBeforeCheck)
    assert.equal(render().pendingBot, null)
    createMode = "success"
    await render().createBot({ name: "corrected project", instructions: "" })
    assert.equal(render().pendingBot, null)
    assert.equal(render().selectedBot?.name, "corrected project")
    assert.equal(messages.length, 1, "Bot creation and recovery never re-post the stored input")

    await t.test("new connections wait for their own complete read and retain unknown writes without replay", async () => {
      knownBots = [bot]
      await render().refresh()
      assert.equal(render().pendingMessages.length, 0)
      for (const outcome of ["success", "failure"]) {
        const write = deferred(); messageHold = write
        const before = messages.length
        assert.equal(await render().sendMessage("reconnect payload"), true, "local acceptance does not wait for the entry RPC")
        await drainOutbox()
        assert.equal(render().messageBusy, false)
        const original = render().pendingMessages.find((item) => item.text === "reconnect payload")
        assert.ok(original)
        assert.equal(messages.length, before + 1)
        const reading = deferred(); conversationHold = reading
        const ackBefore = acknowledgements.length
        unreadNotifications = [{ notification_id: `note-${activeConnection.generation}`, kind: "result", text: "unread", conversation_id: "chat", target: { kind: "bot_main", bot_id: "bot" } }]
        const next: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === next }
        activeConnection = next
        assert.equal(render().writable, false)
        await drainOutbox()
        assert.equal(render().canSaveMessages, false, "the new connection must finish its own authorized read")
        await assert.rejects(render().sendMessage("new unread connection"), /先读取|补读/)
        assert.equal(messages.length, before + 1)
        if (outcome === "success") reading.resolve({ ...context, scheduler: null, works: [work] })
        else reading.reject(new Error("new read unavailable"))
        await drainOutbox()
        assert.equal(render().writable, outcome === "success")
        assert.equal(render().canSaveMessages, outcome === "success")
        assert.equal(acknowledgements.length, ackBefore)
        if (outcome === "failure") await assert.rejects(render().sendMessage("new unread connection"), /先读取|补读/)
        conversationHold = null; scheduler = null
        await render().run(() => render().refresh())
        assert.equal(render().writable, true)
        assert.equal(render().pendingMessages.some((item) => item.message_id === original.message_id), true)
        messageHold = null
        if (outcome === "success") write.resolve({ accepted: true, state: "queued", message_id: original.message_id })
        else write.reject(new Error("old response lost"))
        await drainOutbox()
        assert.equal(messages.length, before + 1, "reconnect queries the original input without replaying its write")
        transcript = [...transcript, posted(original.message_id)]
        await render().queryInput(original)
        await render().refresh()
        assert.equal(render().pendingMessages.some((item) => item.message_id === original.message_id), false)
      }
      messageHold = null
      conversationHold = deferred()
      const next: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === next }
      activeConnection = next; render(); await drainOutbox()
      conversationHold.resolve({ ...context, scheduler: null, works: [work] })
      await drainOutbox()
      assert.equal(render().writable, true)
      conversationHold = null
      await render().createBot({ name: "unrelated new bot", instructions: "" })
      assert.equal(render().selectedBot?.name, "unrelated new bot")
      const currentId = render().conversationId, ackBefore = acknowledgements.length
      unreadNotifications = [{ notification_id: "after-reconnect", kind: "result", text: "unread", conversation_id: currentId, target: { kind: "bot_main", bot_id: render().selectedBot!.bot_id } }]
      conversationHold = deferred()
      const restored: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === restored }
      activeConnection = restored; render(); await drainOutbox()
      conversationHold.resolve({ ...context, conversation: { ...context.conversation, conversation_id: currentId }, scheduler: null })
      await drainOutbox()
      assert.equal(render().writable, true)
      assert.equal(acknowledgements.length, ackBefore)
      conversationHold = null
      await render().refresh()
      assert.deepEqual(acknowledgements.slice(ackBefore), ["after-reconnect"])
    })

    await t.test('opening Bot Main acknowledges only its typed Main notices and leaves Task and unknown scope unread', async () => {
      knownBots = [bot]
      await render().refresh()
      render().setView('chat')
      const before = acknowledgements.length
      const writes = messages.length
      unreadNotifications = [
        { notification_id: 'task-scope-notice', kind: 'result', text: '任务更新', conversation_id: 'chat', target: { kind: 'task', task_id: 'workspace-task' } },
        { notification_id: 'unknown-scope-notice', kind: 'result', text: '归属未提供', conversation_id: 'chat' },
        { notification_id: 'main-scope-notice', kind: 'result', text: '主聊天更新', conversation_id: 'chat', target: { kind: 'bot_main', bot_id: 'bot' } },
      ]
      await render().refresh()
      assert.deepEqual(acknowledgements.slice(before), ['main-scope-notice'])
      assert.deepEqual(render().notifications.map((item) => item.notification_id), ['task-scope-notice', 'unknown-scope-notice'])
      await render().markNotificationsRead(['task-scope-notice', 'unknown-scope-notice'])
      assert.deepEqual(acknowledgements.slice(before), ['main-scope-notice', 'task-scope-notice', 'unknown-scope-notice'])
      assert.equal(messages.length, writes, 'reading notifications does not answer or continue a task')
    })

    await t.test('three inputs save before the first RPC returns and reach their target in order without waiting for refresh', async () => {
      knownBots = [bot]
      await render().refresh()
      const before = messages.length
      const firstRpc = deferred()
      messageHold = firstRpc
      assert.deepEqual(await Promise.all([render().sendMessage('input one'), render().sendMessage('input two'), render().sendMessage('input three')]), [true, true, true])
      await drainOutbox()
      const saved = render().pendingMessages
      assert.deepEqual(saved.map((item) => item.text), ['input one', 'input two', 'input three'])
      assert.equal(new Set(saved.map((item) => item.message_id)).size, 3)
      assert.equal(messages.length, before + 1, 'only the first write is in flight for this target')
      const read = deferred(); conversationHold = read
      const refreshing = render().refresh()
      await drainOutbox()
      messageHold = null
      transcript = [...transcript, posted(saved[0].message_id)]
      firstRpc.resolve({ accepted: true, state: 'steering', message_id: saved[0].message_id })
      await drainOutbox()
      assert.deepEqual(messages.slice(before).map((message) => message.text), ['input one', 'input two', 'input three'])
      assert.deepEqual(messages.slice(before).map((message) => message.message_id), saved.map((item) => item.message_id))
      read.resolve({ ...context, messages: transcript, scheduler: null, works: [work] })
      await refreshing
      conversationHold = null
      await render().refresh()
      assert.equal(render().pendingMessages.length, 0)
    })

    await t.test('two tabs preserve target order while the earlier input is still signing', { timeout: 10_000 }, async () => {
      const otherHooks = hookHarness()
      const loadOther = moduleLoader(sandbox, (specifier, filename) => {
        if (specifier === 'react') return otherHooks.react
        if (specifier.endsWith('/lib/node.ts')) return { ...nodeSettings, messageOf: (cause: unknown) => cause instanceof Error ? cause.message : String(cause) }
        if (specifier === 'react/jsx-runtime') return { jsx: () => null }
        return loadOther(resolve(dirname(filename), specifier))
      }, (filename) => readFileSync(filename, 'utf8') + (filename.endsWith('/workspace/context.tsx') ? '\nexport { useWorkspaceState }\n' : ''))
      const otherModule = loadOther(resolve(clientRoot, 'workspace/context.tsx'))
      const otherHook = otherModule.useWorkspaceState
      assert.ok(typeof otherHook === 'function')
      const follower = { leader: () => false, post() {}, listen: () => () => {} }
      const other = () => {
        otherHooks.reset()
        const state: Workspace = otherHook(platform, activeConnection, undefined, follower)
        otherHooks.afterRender.splice(0).forEach((run) => run())
        return state
      }
      const sign = platform.transport.signClient, gate = deferred()
      let signing = false
      platform.transport.signClient = async (domain, payload) => {
        if (new TextDecoder().decode(payload).includes('"text":"first tab input"')) { signing = true; await gate.promise }
        return sign(domain, payload)
      }
      const previousBots = knownBots, previousTranscript = transcript, previousMessages = messages.length
      const previousOutbox = new Map(outboxItems)
      const submitted = deferred(), sendingConnection = activeConnection, request = sendingConnection.rpc
      sendingConnection.rpc = async (method, params) => {
        const result = await request(method, params)
        if (method === 'message' && params?.text === 'second tab input') submitted.resolve(result)
        return result
      }
      const independentBot = { ...bot, bot_id: 'other-tab-bot', conversation_id: 'other-tab-chat' }
      let first: Promise<boolean> | undefined, second: Promise<boolean> | undefined, sameTabSecond: Promise<boolean> | undefined, independent: Promise<boolean> | undefined
      try {
        knownBots = [bot, independentBot]
        await render().refresh()
        other(); other().setView('chat'); other()
        await drainOutbox(); other(); await drainOutbox()
        assert.equal(other().writable, true)
        const before = messages.length
        first = other().sendMessage('first tab input')
        await drainOutbox()
        assert.equal(signing, true, 'the follower reserved its sequence and is waiting on signing')
        independent = render().sendMessage('independent tab input', [], [], { target: { kind: 'bot_main', bot_id: independentBot.bot_id }, conversation_id: independentBot.conversation_id })
        await drainOutbox()
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['independent tab input'], 'a different target must remain independent of the held signature')
        sameTabSecond = other().sendMessage('earlier same tab second input')
        await drainOutbox()
        second = render().sendMessage('second tab input')
        await drainOutbox()
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['independent tab input'], 'a later tab cannot submit ahead of the input still being signed')
        gate.resolve(null)
        assert.deepEqual(await Promise.all([first, sameTabSecond, second, independent]), [true, true, true, true])
        await submitted.promise
        await render().refresh(); await drainOutbox()
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['independent tab input', 'first tab input', 'earlier same tab second input', 'second tab input'])
        assert.equal(Number(messages[before + 2].sequence), Number(messages[before + 1].sequence) + 1)
        assert.equal(Number(messages[before + 3].sequence), Number(messages[before + 2].sequence) + 1)
      } finally {
        gate.resolve(null)
        await Promise.allSettled([first, sameTabSecond, second, independent])
        sendingConnection.rpc = request
        platform.transport.signClient = sign
        knownBots = previousBots
        for (const effect of otherHooks.effects.values()) effect.cleanup?.()
        await render().refresh(); await drainOutbox()
        outboxItems.clear()
        for (const [key, item] of previousOutbox) outboxItems.set(key, item)
        transcript = previousTranscript
        messages.splice(previousMessages)
        await render().refresh()
      }
    })

    await t.test('attachment upload holds target order while later drafts remain locally recoverable', async () => {
      const before = messages.length
      const uploading = deferred(); uploadHold = uploading
      const bytes = new TextEncoder().encode('attachment body')
      const file = { name: 'notes.txt', arrayBuffer: async () => bytes.buffer }
      assert.equal(await render().sendMessage('attachment first', [file]), true)
      assert.equal(await render().sendMessage('text second'), true)
      assert.equal(await render().sendMessage('text third'), true)
      await drainOutbox()
      const saved = render().pendingMessages
      assert.deepEqual(saved.map((item) => item.text), ['attachment first', 'text second', 'text third'])
      assert.equal(saved[0].attachments?.[0]?.name, 'notes.txt')
      assert.equal(messages.length, before, 'the later text cannot pass the earlier attachment')
      await reload()
      assert.deepEqual(render().pendingMessages.map((item) => item.message_id), saved.map((item) => item.message_id), 'reload retains each original input identity')
      uploadHold = null; uploading.resolve({ complete: true })
      await drainOutbox()
      assert.deepEqual(messages.slice(before).map((message) => message.text), ['attachment first', 'text second', 'text third'])
      await render().refresh()
      assert.equal(render().pendingMessages.length, 0)
    })

    await t.test('a locally created channel task receives follow-ups before its create command is acknowledged', { timeout: 10_000 }, async () => {
      const current = activeConnection, originalRpc = current.rpc
      const originalAll = platform.outbox.all, checking = deferred(), continueCheck = deferred()
      const creating = deferred(), creationStarted = deferred()
      const commands: Record<string, unknown>[] = []
      const saved: PendingMessage[] = []
      const before = messages.length
      let domain: WorkspaceDomainState = {
        channels: [{ channel_id: 'channel', title: '频道', purpose: '', members: ['bot'], coordinator_bot: 'bot', coordinator_node: 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', owner_epoch: 1, revision: 1, event_hash: 'channel-event', created_at_ms: 1 }],
        tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false },
      }
      current.rpc = async (method, body = {}) => {
        if (method === 'workspace_state') return domain
        if (method === 'workspace_command') {
          const command = workspaceModel.parseWorkspaceCommand(body.command)
          assert.ok(command.kind === 'create_task')
          assert.ok(command.expected_result.trim(), 'Node valid_text rejects an empty expected result')
          assert.ok(new TextEncoder().encode(command.expected_result).length <= 16384)
          commands.push(JSON.parse(JSON.stringify(body))); creationStarted.resolve(null); return creating.promise
        }
        if (method === 'workspace_history') return {
          history_authority: 'authority', next_cursor: null, complete: true, events: messages.slice(before).map((message, index) => ({ event_hash: String(message.message_id), created_at_ms: index + 1, scope_id: 'chat', kind: 'input_posted', speaker: { kind: 'human', device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' }, body: { ...message, type: 'input_posted' } })),
          coverage: { limit: 128, truncated: false, scope: 'task' },
        }
        return originalRpc(method, body)
      }
      try {
        await render().readWorkspaceState()
        assert.equal(await render().sendMessage('channel one', [], undefined, { target: { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, onSaved: (input) => { saved.push(input) } }), true)
        const first = saved[0]
        assert.ok(first)
        assert.ok(first.target.kind === 'channel_task')
        assert.equal(first.expected_revision, 1)
        assert.equal(first.conversation_id, 'chat')
        await creationStarted.promise
        const target = first.target
        const plainTarget = JSON.parse(JSON.stringify(target))
        let holdNextCheck = true
        platform.outbox.all = async () => {
          const hold = holdNextCheck
          holdNextCheck = false
          if (hold) { checking.resolve(null); await continueCheck.promise }
          return originalAll()
        }
        const second = render().sendMessage('channel two', [], undefined, { target })
        await checking.promise
        const third = render().sendMessage('channel three', [], undefined, { target, expected_revision: first.expected_revision, conversation_id: first.conversation_id })
        await drainOutbox()
        continueCheck.resolve(null)
        assert.deepEqual(await Promise.all([second, third]), [true, true])
        await drainOutbox()
        assert.equal(commands.length, 1, 'only the original creation is submitted while its acknowledgement is held')
        assert.equal(messages.length, before, 'input delivery waits behind its real task creation')
        assert.deepEqual(JSON.parse(JSON.stringify(render().pendingMessages.map((input) => input.target))), [plainTarget, plainTarget, plainTarget])
        const command = workspaceModel.parseWorkspaceCommand(commands[0].command)
        assert.ok(command.kind === 'create_task')
        assert.equal(command.task_id, target.task_id)
        const event = 'c'.repeat(64)
        domain = { ...domain, tasks: [{ ...command, root_task_id: command.task_id, revision: 1, action: 'continue', request_event: event, event_hash: event, created_at_ms: 1, updated_at_ms: 1, state: 'not_started' }] }
        creating.resolve({ ...domain, command_id: commands[0].command_id, event_hash: event, duplicate: false })
        await drainOutbox()
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['channel one', 'channel two', 'channel three'])
        assert.deepEqual(messages.slice(before).map((message) => message.target), [plainTarget, plainTarget, plainTarget])
        await render().readWorkspaceHistory({ task_id: target.task_id })
        assert.equal(render().pendingMessages.length, 0)
      } finally { continueCheck.resolve(null); platform.outbox.all = originalAll; current.rpc = originalRpc }
    })

    await t.test('Main and Work NewTask, explicit or from the prompt, use a real valid expected result and preserve Work sources', async () => {
      const current = activeConnection, originalRpc = current.rpc
      const sourceEvents = ['a'.repeat(64)]
      for (const kind of ['main', 'work', 'prompt']) {
        const before = messages.length, commands: Extract<import('@vibeface/client/workspace/types.ts').WorkspaceCommand, { kind: 'create_task' }>[] = []
        let domain: WorkspaceDomainState = { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } }
        current.rpc = async (method, body = {}) => {
          if (method === 'workspace_state') return domain
          if (method === 'work_detail') return { work: { ...work, conversation_id: 'chat', target: null, source_events: sourceEvents }, coverage: { source: 'shared_history', complete: false } }
          if (method === 'workspace_command') {
            const command = workspaceModel.parseWorkspaceCommand(body.command)
            assert.ok(command.kind === 'create_task')
            assert.ok(command.expected_result.trim(), 'the real Node requires nonempty expected_result')
            assert.ok(new TextEncoder().encode(command.expected_result).length <= 16384)
            assert.deepEqual(command.source_events, kind === 'work' ? sourceEvents : [])
            commands.push(command)
            const event = 'd'.repeat(64)
            domain = { ...domain, tasks: [{ ...command, root_task_id: command.task_id, revision: 1, action: 'continue', request_event: event, event_hash: event, created_at_ms: 1, updated_at_ms: 1, state: 'not_started' }] }
            return { ...domain, command_id: body.command_id, event_hash: event, duplicate: false }
          }
          if (method === 'workspace_history') return { history_authority: 'authority', next_cursor: null, complete: true, events: messages.slice(before).map((message, index) => ({ event_hash: String(message.message_id), created_at_ms: index + 1, scope_id: 'chat', kind: 'input_posted', speaker: { kind: 'human', device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' }, body: { ...message, type: 'input_posted' } })), coverage: { limit: 128, truncated: false, scope: 'task' } }
          return originalRpc(method, body)
        }
        try {
          await render().readWorkspaceState()
          if (kind === 'work') await render().readWork({ conversation_id: 'chat', work_id: work.work_id })
          let saved: PendingMessage | undefined
          const target: InputTarget = kind === 'work' ? { kind: 'work', work_id: work.work_id } : { kind: 'bot_main', bot_id: bot.bot_id }
          const text = kind === 'prompt' ? '新任务：prompt original request' : `${kind} new task original request`
          assert.equal(await render().sendMessage(text, [], undefined, { target, intent: kind === 'prompt' ? promptIntent(text) : 'new_task', onSaved: (input) => { saved = input } }), true)
          await settleComponent()
          assert.ok(saved && saved.target.kind === 'task')
          assert.equal(commands.length, 1)
          assert.equal(commands[0].task_id, saved.target.task_id)
          assert.equal(messages.length, before + 1)
          assert.equal((messages.at(-1)?.target as { task_id: string }).task_id, saved.target.task_id)
          assert.equal(messages.at(-1)?.intent, 'new_task', `${kind} input opens a new task`)
          await render().readWorkspaceHistory({ task_id: saved.target.task_id })
        } finally { current.rpc = originalRpc }
      }
      const before = messages.length, commands: unknown[] = []
      current.rpc = async (method, body = {}) => {
        if (method === 'workspace_command') commands.push(body.command)
        return originalRpc(method, body)
      }
      try {
        let saved: PendingMessage | undefined
        const target: InputTarget = { kind: 'bot_main', bot_id: bot.bot_id }
        assert.equal(await render().sendMessage('新任务：程序化交代仍是普通输入', [], undefined, { target, onSaved: (input) => { saved = input } }), true)
        await settleComponent()
        assert.deepEqual(commands, [], 'a programmatic send without an intent never creates a task')
        assert.deepEqual(JSON.parse(JSON.stringify(saved?.target)), target)
        assert.equal(messages.length, before + 1)
        assert.equal(messages.at(-1)?.intent, 'continue', 'only composers read the prompt prefix')
      } finally { current.rpc = originalRpc }
    })

    await t.test('Task and Channel attachment reads follow the signed event scope while Main stays on another Bot', async () => {
      const current = activeConnection, originalRpc = current.rpc
      const otherBot = { ...bot, bot_id: 'attachment-bot', conversation_id: 'attachment-chat', name: '附件 Bot' }
      knownBots = [bot, otherBot]
      await render().refresh(); await render().selectBot(bot)
      const bytes = new TextEncoder().encode('test')
      const hash = [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))].map((byte) => byte.toString(16).padStart(2, '0')).join('')
      const calls: Record<string, unknown>[] = []
      let delayedMainRead: ReturnType<typeof deferred> | null = null
      let target: InputTarget = { kind: 'task', task_id: 'attachment-task' }
      current.rpc = async (method, body = {}) => {
        if (method === 'workspace_history') return { history_authority: 'authority', next_cursor: null, complete: true, events: [{ event_hash: 'attachment-event', created_at_ms: 1, scope_id: otherBot.conversation_id, kind: 'input_posted', speaker: { kind: 'human', device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' }, body: { type: 'input_posted', report_results: true, message_id: 'attachment-input', target, intent: 'continue', sequence: 1, expected_revision: 1, text: '任务附件', attachments: [{ hash, size: bytes.length, name: 'scope.txt' }] } }], coverage: { limit: 128, truncated: false, scope: target.kind === 'task' ? 'task' : 'channel' } }
        if (method === 'read_artifact') {
          calls.push(JSON.parse(JSON.stringify(body)))
          if (delayedMainRead) return delayedMainRead.promise
          assert.equal(body.conversation_id, otherBot.conversation_id, 'the fake Node enforces attachment ownership')
          return { blob_hash: hash, offset: 0, size: bytes.length, next_offset: null, ...(body.binary ? { data: btoa('test') } : { text: 'test' }) }
        }
        return originalRpc(method, body)
      }
      try {
        for (const channel of [false, true]) {
          target = channel ? { kind: 'channel_task', channel_id: 'attachment-channel', task_id: 'attachment-task', bot_id: otherBot.bot_id } : { kind: 'task', task_id: 'attachment-task' }
          const shared = { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => render() }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: activeConnection, directory: null }) }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey } }
          const scope = componentHarness('./workspace-chat.tsx', shared, { setInterval: () => 1, clearInterval() {} }, 'ScopedConversation')
          const props = { target, conversationId: otherBot.conversation_id, label: '附件任务', ...(channel ? { channelId: 'attachment-channel' } : {}) }
          const chat = componentHarness('../../components/arc/conversation-thread.tsx', { '@vibeface/client/chat/timeline.ts': { messageTimes: () => new Map() }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } })
          const message = componentHarness('../chat/elements/message.tsx', { ...shared, '@vibeface/client/chat/protocol.ts': { promptIntent, messageKey, replyStatusLabel }, '@vibeface/client/lib/errors.ts': errors })
          let downloads = 0
          const artifact = componentHarness('../chat/elements/artifact.tsx', { ...shared, '@vibeface/client/workspace/model.ts': { formatSize } }, { crypto, Blob, URL: { createObjectURL: () => 'blob:fixture', revokeObjectURL() {} }, document: { createElement: () => ({ click() { downloads++ } }) }, window: { setTimeout: () => 1 } })
          try {
            scope.render('ScopedConversation', props); await settleComponent()
            const chatProps = scope.render('ScopedConversation', props).find(({ type }) => type === 'Chat')?.props
            assert.ok(chatProps)
            const messageProps = chat.render('Chat', chatProps).find(({ type }) => type === 'Message')?.props
            assert.ok(messageProps); assert.equal(messageProps.conversationId, otherBot.conversation_id)
            const artifactProps = message.render('Message', messageProps).find(({ type }) => type === 'Artifact')?.props
            assert.ok(artifactProps); assert.equal(artifactProps.conversationId, otherBot.conversation_id)
            const controls = () => artifact.render('Artifact', artifactProps)
            const open = controls().find(({ props }) => props.label === '打开')?.props.action
            assert.ok(open); await open()
            assert.equal(controls().find(({ type }) => type === 'pre')?.props.children, 'test', String(render().error))
            const download = controls().find(({ props }) => props.label === '下载')?.props.action
            assert.ok(download); await download(); assert.equal(downloads, 1)
            assert.equal(render().conversationId, bot.conversation_id, 'attachment reads do not select another Main')
          } finally { scope.close(); chat.close(); message.close(); artifact.close() }
        }
        assert.equal(calls.length, 4)
        assert.ok(calls.every((call) => call.conversation_id === otherBot.conversation_id))
        delayedMainRead = deferred()
        const oldMain = render().readArtifact(hash)
        await render().selectBot(otherBot)
        delayedMainRead.resolve({ blob_hash: hash, offset: 0, size: bytes.length, next_offset: null, text: 'test' })
        await assert.rejects(oldMain, /对话已切换/)
        delayedMainRead = null
        await render().selectBot(bot)
      } finally { current.rpc = originalRpc; knownBots = [bot]; await render().refresh() }
    })

    await t.test('retained native chat focus restores its route through the real live bridge without acknowledging the previously selected Bot', async () => {
      const botA = { ...bot, name: 'Bot A' }, botB = { ...bot, bot_id: 'bot-b', conversation_id: 'chat-b', name: 'Bot B' }
      knownBots = [botA, botB]
      knownNodes = [{ ...parityNode, device_id: 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc' }]
      questionRows = []
      conversationMessages = Object.fromEntries([botA, botB].map((item) => [item.conversation_id, [{ message_id: `${item.bot_id}-message`, event_hash: `${item.bot_id}-event`, role: 'user', text: `${item.name} 的消息`, created_at_ms: 1 }]]))
      render().setView('other')
      await render().selectBot(botA)
      let shared = render()
      let joinArguments: unknown[] = []
      const device = { get ready() { return activeConnection }, phase: 'ready', link: { kind: 'online' }, entries: [], directory: null, identity: { deviceId: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' }, async join(...args: unknown[]) { joinArguments = args }, cancelJoin() {}, leave() {} }
      const fileReads = new Map<string, ReturnType<typeof deferred>>()
      class SelectedFile { readonly uri: string; constructor(uri: string) { this.uri = uri } arrayBuffer() { const read = fileReads.get(this.uri); assert.ok(read); return read.promise } }
      const bridge = componentHarness('../../../../../apps/mobile/src/data/live.tsx', {
        'expo-file-system': { File: SelectedFile },
        '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/identity/access.ts': { accessOf },
        '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/lib/node.ts': { entryAddress },
        '@vibeface/client/chat/protocol.ts': { promptIntent, botReach, composerHint }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, messageTimes, taskMessage },
        '@vibeface/client/workspace/home.ts': { mainConversationAttention },
        '@vibeface/client/workspace/bot-settings.ts': { botCopy }, '@vibeface/client/workspace/inbox.ts': { inboxItems }, '@vibeface/client/workspace/labels.ts': taskLabels,
        '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers }, '@vibeface/client/workspace/storage.ts': { storageOf },
        '@/platform/remote-notifications': { remoteNotifications: { sync: async () => {} } },
        'expo-notifications': { setBadgeCountAsync: async () => {}, addPushTokenListener: () => ({ remove() {} }) },
        '@/platform': { mobilePlatform: { storage: browser.localStorage, onForeground: () => () => {} } }, './context': { WorkspaceProvider: 'NativeWorkspace' }, './last-seen': lastSeenModel,
      }, {}, 'Live')
      type NativeWorkspace = import('../../../../../apps/mobile/src/data/types.ts').Workspace
      const mainSaves: ReturnType<NativeWorkspace['actions']['send']>[] = []
      let native: NativeWorkspace
      const mobile = (liveProgress?: typeof shared.liveProgress) => {
        shared = render()
        if (liveProgress) shared = { ...shared, liveProgress }
        const value = bridge.render('Live').find(({ type }) => type === 'NativeWorkspace')?.props.value
        assert.ok(value && typeof value === 'object')
        native = value as NativeWorkspace
        return native
      }
      const online = mobile()
      const bind = () => {}
      await online.actions.join('vibeface://join/fixture', '手机', bind)
      assert.deepEqual(joinArguments, ['vibeface://join/fixture', '手机', bind])
      let routine: string | undefined
      const nativeNavigations: string[] = []
      const platform = { OS: 'android' }, insets = { bottom: 0 }, keyboard = { height: { value: 0 }, progress: { value: 0 } }
      let glass = false, theme = 'light'
      let dockHeight: { value: number } | undefined, emptyPadding = () => ({ paddingBottom: 0 })
      const chat = componentHarness('../../../../../apps/mobile/app/b/[botId].tsx', {
        'expo-file-system': { File: SelectedFile },
        'expo-router': { Stack: { Screen: 'Screen' }, router: { push: (target: string) => { nativeNavigations.push(target) } }, useLocalSearchParams: () => ({ botId: botA.bot_id, routine }) },
        '@vibeface/client/lib/node.ts': { messageOf },
        '@/platform': { mobilePlatform: { storage: browser.localStorage } },
        'expo-router/react-navigation': { useHeaderHeight: () => platform.OS === 'ios' ? 96 : 0 },
        'react-native': { Platform: platform, FlatList: 'FlatList', View: 'View', StyleSheet: { absoluteFill: {} } },
        'react-native-keyboard-controller': { KeyboardChatScrollView: 'KeyboardChatScrollView', KeyboardStickyView: 'KeyboardStickyView', useReanimatedKeyboardAnimation: () => keyboard },
        'react-native-reanimated': { default: { View: 'AnimatedView' }, useAnimatedStyle: (read: typeof emptyPadding) => { emptyPadding = read; return read() }, useSharedValue: (initial: number) => dockHeight ??= { value: initial } },
        'react-native-safe-area-context': { useSafeAreaInsets: () => insets, useSafeAreaFrame: () => ({ width: 390 }) },
        'uniwind': { withUniwind: (component: unknown) => component, useUniwind: () => ({ theme }), useCSSVariable: () => theme === 'dark' ? 'black' : 'white' },
        '@/data/context': { useWorkspace: () => native }, '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
        '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/bot-settings.ts': { botCopy, scheduleName }, '@vibeface/client/workspace/home.ts': { unreachable },
        '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice: () => false, botReach, botCanSend, composerHint, inputTargetKey, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { messageTimes: () => new Map() },
        '@/components/bot-face': { BotFace: 'BotFace', ScreenFaces: 'ScreenFaces', botMood: () => 'idle' }, '@/components/kit': { botTint: () => ({}), EmptyState: 'EmptyState' },
        '@/features/chat/work': { useChatExtras: () => ({ first: [], after: new Map(), rest: [], cards: [] }) }, '@/lib/layout': { useWide: () => false },
        '@/features/chat/chrome': { ChatMaterial: 'ChatMaterial', useChatChrome: () => glass ? { kind: 'glass', colorScheme: theme } : { kind: 'solid' } },
      }, { requestAnimationFrame: (run: () => void) => run(), setInterval: () => 1, clearInterval() {} })
      const stack = { Screen: 'Screen', Protected: 'Protected' }
      const layout = componentHarness('../../../../../apps/mobile/app/_layout.tsx', {
        'expo-router': { Stack: stack, useRouter: () => ({}), usePathname: () => '/' }, 'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) }, 'expo-linking': { useLinkingURL: () => null }, '@/data/live': { LiveWorkspace: 'LiveWorkspace', useLive: () => shared, useWaiting: () => 0 },
        '@/data/context': { useWorkspace: () => ({ session: { phase: 'ready' } }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
        'uniwind': { useCSSVariable: () => ['white', 'black'] }, '@/lib/layout': { useWide: () => false }, 'react-native': { Platform: { OS: 'ios' }, View: 'View' },
      })
      try {
        const originalMessages = conversationMessages, originalScheduler = scheduler
        const input = { message_id: 'host-input', event_hash: 'host-input-event', role: 'user', text: '主输入', created_at_ms: 1 }
        const followup = { message_id: 'steered-input', event_hash: 'steered-input-event', role: 'user', text: '补充输入', created_at_ms: 2 }
        const reply = { message_id: 'host-reply', event_hash: 'host-reply-event', role: 'assistant', text: '共同回复', in_reply_to: input.message_id, status: 'completed', created_at_ms: 3 }
        try {
          for (const outcome of ['answered', 'unfinished', 'unknown']) {
            conversationMessages = { chat: [input, followup] }
            const host = { scope: { kind: 'main' }, trigger: 'message:host-input', state: 'running', message_id: input.message_id, reply_message_id: reply.message_id }
            scheduler = { scope_jobs: [], unanswered: [], capabilities: { cancel: true, streaming: true, steer: true }, jobs: [
              host,
              { scope: { kind: 'main' }, trigger: 'message:steered-input', state: 'steered', outcome, message_id: followup.message_id, reply_message_id: 'unused-steered-reply' },
            ] }
            await render().refresh()
            assert.equal(mobile().conversations.chat.replying, true, 'the host turn still runs after accepting a supplement')
            assert.equal(mobile().conversations.chat.stoppable, true)
            const streaming = mobile([{ kind: 'manager', key: 'host-progress', conversation_id: 'chat', trigger: host.trigger, source_node: 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', text: '部分回复', sequence: 1, items: [], needsRefresh: false }]).conversations.chat.messages.find((message) => message.state === 'streaming')
            assert.equal(streaming?.in_reply_to, input.message_id, 'the streamed reply retains its host input after a supplement')
            host.state = 'completed'
            await render().refresh()
            assert.equal(mobile().conversations.chat.replying, true, 'a completed host keeps its activity until its reply syncs')
            conversationMessages.chat.push(reply)
            await render().refresh()
            const completed = mobile().conversations.chat
            assert.equal(completed.messages.some((message) => message.message_id === reply.message_id), true)
            assert.equal(completed.replying, false, `a ${outcome} supplement never waits for an independent reply`)
            assert.equal(completed.stoppable, false)
          }
          conversationMessages = { chat: [input] }
          for (const state of ['publishing', 'completed', 'failed', 'interrupted', 'cancelled', 'rejected', 'reconciling']) {
            scheduler = { scope_jobs: [], unanswered: [], jobs: [{ scope: { kind: 'main' }, trigger: 'message:host-input', state, message_id: input.message_id, reply_message_id: reply.message_id }] }
            await render().refresh()
            assert.equal(mobile().conversations.chat.replying, !['cancelled', 'rejected', 'reconciling'].includes(state), `${state} waits only when the job produces its own reply`)
          }
        } finally {
          conversationMessages = originalMessages; scheduler = originalScheduler
          await render().refresh(); mobile()
        }
        const tree = layout.render('default')
        const screens = tree.find(({ type }) => typeof type === 'function' && type.name === 'Screens')
        assert.ok(screens && typeof screens.type === 'function')
        screens.type(screens.props)
        const listener = tree.find(({ type }) => type === stack)?.props.screenListeners
        assert.ok(typeof listener === 'function')
        const focus = (name: string) => listener({ route: { name } }).focus()
        chat.render('default')
        await settleComponent()
        mobile()
        assert.equal(shared.conversationId, 'chat')
        chat.blur()
        focus('tasks')
        unreadNotifications = [
          { notification_id: 'native-a', kind: 'result', text: 'A 的通知', conversation_id: 'chat', target: { kind: 'bot_main', bot_id: botA.bot_id } },
          { notification_id: 'native-b', kind: 'result', text: 'B 的通知', conversation_id: 'chat-b', target: { kind: 'bot_main', bot_id: botB.bot_id } },
        ]
        await render().selectBot(botB)
        mobile()
        focus('b/[botId]/work/[workId]')
        await render().refresh()
        focus('tasks')
        const ackBefore = acknowledgements.length
        focus('b/[botId]')
        await render().refresh()
        assert.deepEqual(acknowledgements.slice(ackBefore), [], 'a Stack focus before the retained chat callback cannot acknowledge Bot B')
        mobile()
        chat.focus()
        await settleComponent()
        mobile()
        const elements = chat.render('default')
        assert.equal(shared.conversationId, 'chat', 'the retained A route reselects A without changing its id or remounting')
        assert.equal(shared.view, 'chat')
        const rows = elements.find(({ type }) => type === 'FlatList')?.props.data as { message?: { text?: string } }[]
        assert.equal(rows[0]?.message?.text, 'Bot A 的消息')
        assert.equal(elements.find(({ type }) => type === 'Composer')?.props.blocked, null)
        const sentOptions: (InputOptions | undefined)[] = []
        const currentNative = mobile()
        const originalSend = currentNative.actions.send
        currentNative.actions.send = async (_id, _text, _files, options) => { sentOptions.push(options) }
        try {
          routine = 'schedule:not-loaded'
          chat.render('default')
          const composer = chat.render('default').find(({ type }) => type === 'Composer')?.props
          composer?.onChange?.('补充这个例行任务')
          const send = chat.render('default').find(({ type }) => type === 'Composer')?.props.onSend
          assert.ok(typeof send === 'function'); send(); await settleComponent()
          assert.deepEqual(JSON.parse(JSON.stringify(sentOptions[0]?.target)), { kind: 'routine', bot_id: botA.bot_id, trigger: routine }, 'a missing observation must not redirect input to Main')
        } finally {
          currentNative.actions.send = originalSend
          routine = undefined
          chat.render('default'); chat.render('default')
        }
        const mainActions = mobile().actions, originalMainSend = mainActions.send
        mainActions.send = (...args) => { const saved = originalMainSend(...args); mainSaves.push(saved); return saved }
        const composer = () => chat.render('default').find(({ type }) => type === 'Composer')?.props
        const attach = (uri: string) => { const remove = composer()?.onFiles; assert.ok(typeof remove === 'function'); remove([]); const input = composer()?.onPickFiles; assert.ok(typeof input === 'function'); const files = [{ uri, name: uri.split('/').at(-1) ?? 'file', size: 3, mimeType: null }]; input(files.map(file => ({ ...file, arrayBuffer: async () => { const held = fileReads.get(uri); assert.ok(held); return await held.promise }, text: async () => '', dispose() {} }))); return files }
        const sendDraft = () => { const send = composer()?.onSend; assert.ok(typeof send === 'function'); send() }
        const storedDraft = () => chat.drafts.read({ networkId: fixtureNetwork, deviceId: fixtureClient, conversationId: 'chat', inputTarget: routine ? `routine:${routine}` : 'main' }).text
        const preparing = deferred(); fileReads.set('file:///first.txt', preparing)
        const sentBefore = messages.length
        composer()?.onChange?.('原草稿仍在准备附件')
        attach('file:///first.txt'); sendDraft(); sendDraft(); await settleComponent()
        composer()?.onChange?.('原草稿仍在准备附件'); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, 1, 'repeated taps and unchanged text cannot submit the same pending draft twice')
        assert.equal(composer()?.value, '原草稿仍在准备附件')
        assert.equal(storedDraft(), '原草稿仍在准备附件', 'the durable text remains recoverable before local enqueue')
        assert.equal(render().pendingMessages.some((item) => item.text === '原草稿仍在准备附件'), false, 'the file read is still before durable acceptance')
        const preparingLater = deferred(); fileReads.set('file:///later.txt', preparingLater)
        composer()?.onChange?.('后来输入的内容')
        const laterFiles = attach('file:///later.txt')
        sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, 1, 'the native scope retains edits while the first draft is still preparing')
        preparing.resolve(new Uint8Array([1, 2, 3]).buffer)
        await mainSaves[0]; await settleComponent(); mobile()
        assert.equal(composer()?.value, '后来输入的内容')
        assert.deepEqual(JSON.parse(JSON.stringify(composer()?.files)), laterFiles, 'local acceptance cannot remove attachments selected after submission')
        assert.equal(storedDraft(), '后来输入的内容')
        sendDraft(); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, 2, 'the newer draft submits exactly once after the earlier save finishes')
        preparingLater.resolve(new Uint8Array([4, 5, 6]).buffer)
        await mainSaves[1]; await settleComponent(); mobile()
        assert.equal(composer()?.value, '')
        assert.deepEqual(messages.slice(sentBefore).map((message) => message.text), ['原草稿仍在准备附件', '后来输入的内容'])
        assert.equal(new Set(messages.slice(sentBefore).map((message) => message.message_id)).size, 2, 'each edited draft produces exactly one input identity')
        const matching = deferred(); fileReads.set('file:///matching.txt', matching)
        composer()?.onChange?.('保存后才清空')
        attach('file:///matching.txt'); sendDraft(); await settleComponent()
        assert.equal(storedDraft(), '保存后才清空')
        matching.resolve(new Uint8Array([7, 8, 9]).buffer)
        await mainSaves[2]; await settleComponent(); mobile()
        assert.equal(composer()?.value, '')
        const clearedFiles = composer()?.files
        assert.ok(Array.isArray(clearedFiles)); assert.equal(clearedFiles.length, 0)
        assert.equal(storedDraft(), '', 'only the still-matching draft clears after the real client saves it')
        const failedRead = deferred(); fileReads.set('file:///retry.txt', failedRead)
        composer()?.onChange?.('附件失败后可重发')
        attach('file:///retry.txt'); sendDraft(); await settleComponent()
        failedRead.reject(new Error('file unavailable'))
        await assert.rejects(mainSaves[3], /file unavailable/); await settleComponent(); mobile()
        assert.equal(storedDraft(), '附件失败后可重发')
        const retriedRead = deferred(); fileReads.set('file:///retry.txt', retriedRead)
        sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, 5, 'a failed local save releases only its pending draft guard')
        retriedRead.resolve(new Uint8Array([10, 11, 12]).buffer)
        await mainSaves[4]; await settleComponent(); mobile()
        assert.equal(storedDraft(), '')
        assert.equal(messages.slice(sentBefore).filter((message) => message.text === '附件失败后可重发').length, 1)
        const changingOptions = deferred(); fileReads.set('file:///options.txt', changingOptions)
        const optionStart = mainSaves.length, optionMessages = messages.length
        composer()?.onChange?.('切换目标仍是独立输入')
        const optionFiles = attach('file:///options.txt')
        sendDraft(); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, optionStart + 1, 'unchanged repeated taps remain deduplicated')
        const pendingRoutine = 'schedule:pending-draft'
        routine = pendingRoutine
        chat.render('default'); chat.render('default')
        assert.equal(composer()?.value, '', 'a routine opens its own durable draft')
        assert.deepEqual(JSON.parse(JSON.stringify(composer()?.files)), [])
        composer()?.onChange?.('切换目标仍是独立输入'); attach('file:///options.txt')
        sendDraft(); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, optionStart + 2, 'another target can prepare its own draft independently')
        routine = undefined
        chat.render('default'); chat.render('default')
        sendDraft(); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, optionStart + 2, 'returning to Main cannot duplicate its still-pending input')
        routine = pendingRoutine
        chat.render('default'); chat.render('default')
        composer()?.onChange?.('当前例行任务草稿'); await settleComponent()
        changingOptions.resolve(new Uint8Array([13, 14, 15]).buffer)
        await Promise.all(mainSaves.slice(optionStart)); await settleComponent(); mobile()
        assert.equal(composer()?.value, '当前例行任务草稿')
        assert.deepEqual(JSON.parse(JSON.stringify(composer()?.files)), optionFiles, 'an earlier revision cannot clear the current attachment')
        assert.equal(storedDraft(), '当前例行任务草稿')
        const optionMainTarget = { kind: 'bot_main', bot_id: botA.bot_id }, optionRoutineTarget = { kind: 'routine', bot_id: botA.bot_id, trigger: pendingRoutine }
        assert.deepEqual(messages.slice(optionMessages).map(message => JSON.stringify([message.target, message.intent])).sort(), [[optionMainTarget, 'continue'], [optionRoutineTarget, 'continue']].map(value => JSON.stringify(value)).sort(), 'chat input steers by default')
        sendDraft(); await settleComponent(); await mainSaves.at(-1); await settleComponent(); mobile()
        assert.equal(mainSaves.length, optionStart + 3)
        assert.equal(composer()?.value, '')
        assert.equal(storedDraft(), '', 'the matching target still clears after durable acceptance')
        assert.deepEqual(messages.slice(optionMessages).map(message => JSON.stringify([message.target, message.intent])).sort(), [[optionMainTarget, 'continue'], [optionRoutineTarget, 'continue'], [optionRoutineTarget, 'continue']].map(value => JSON.stringify(value)).sort())
        assert.equal(new Set(messages.slice(optionMessages).map(message => message.message_id)).size, 3)
        routine = undefined
        chat.render('default'); chat.render('default')
        mainActions.send = originalMainSend
        for (const change of ['text', 'unchanged']) {
          const actions = mobile().actions, original = actions.send, accepted = deferred(), before = nativeNavigations.length
          let saving = Promise.resolve()
          actions.send = (_id, text, _files, options) => {
            assert.ok(options)
            assert.equal(options.intent, promptIntent(text), 'native Main sends the prompt intent')
            assert.equal(options.intent, 'new_task')
            saving = accepted.promise.then(() => { options.onSaved?.({ report_results: true, message_id: 'native-saved', conversation_id: 'chat', target: { kind: 'task', task_id: 'native-created-task' }, intent: 'continue', expected_revision: 1, sequence: 1, text, attachments: [], created_at_ms: 1, delivery: 'saved' }) })
            return saving
          }
          try {
            composer()?.onChange?.(`新任务 手机 ${change}`); sendDraft()
            if (change === 'text') composer()?.onChange?.('手机仍在编辑')
            chat.render('default')
            accepted.resolve(undefined); await saving; await settleComponent()
            assert.equal(nativeNavigations.length, before + (change === 'unchanged' ? 1 : 0), 'native Main only navigates for its unchanged submitted draft')
            assert.equal(composer()?.value, change === 'unchanged' ? '' : '手机仍在编辑')
            if (change === 'unchanged') assert.equal(nativeNavigations.at(-1), '/task/native-created-task')
          } finally { actions.send = original }
        }

        await render().refresh()
        assert.deepEqual(acknowledgements.slice(ackBefore), ['native-a'])
        assert.equal(render().notifications.some((item) => item.notification_id === 'native-b'), true)

        chat.blur()
        focus('tasks')
        knownBots = [botB]
        await render().refresh()
        const waiting = mobile().actions.open('chat')
        await settleComponent()
        assert.equal(render().view, 'other', 'a route whose Bot has not arrived cannot make the old Bot a visible chat')
        await render().refresh()
        assert.deepEqual(acknowledgements.slice(ackBefore), ['native-a'])
        knownBots = [botA, botB]
        await render().refresh()
        mobile()
        await settleComponent()
        mobile()
        assert.equal(shared.conversationId, 'chat', 'a focused waiting route selects its Bot when the catalog arrives')
        assert.equal(shared.view, 'chat')
        waiting()
        knownBots = [botB]
        await render().refresh()
        const cancelled = mobile().actions.open('chat')
        cancelled()
        knownBots = [botA, botB]
        await render().refresh()
        mobile()
        await settleComponent()
        assert.equal(render().conversationId, 'chat-b', 'a blurred waiting route cannot select its late Bot')
        assert.equal(render().view, 'other')
        conversationHold = deferred()
        const reading = conversationHold
        const backgrounded = mobile().actions.open('chat')
        backgrounded()
        focus('tasks')
        reading.resolve({ ...context, scheduler: null, messages: conversationMessages.chat })
        await settleComponent()
        mobile()
        assert.equal(shared.view, 'tasks', 'a completed read from a blurred chat cannot reactivate the chat view')
        assert.deepEqual(acknowledgements.slice(ackBefore), ['native-a'])
        conversationHold = null

        const android = chat.render('default')
        assert.equal(android.find(({ type }) => type === 'KeyboardStickyView')?.props.style, undefined)
        assert.match(String(android.find(({ type }) => type === 'FlatList')?.props.contentContainerClassName), /py-6/)
        platform.OS = 'ios'; insets.bottom = 34
        const ios = chat.render('default'), flat = ios.find(({ type }) => type === 'FlatList')
        assert.ok(dockHeight)
        assert.equal(JSON.stringify(ios.find(({ type }) => type === 'KeyboardStickyView')?.props.style), JSON.stringify({ position: 'absolute', bottom: 0, left: 0, right: 0 }))
        assert.equal(ios.find(({ type }) => type === 'KeyboardStickyView')?.props.pointerEvents, 'box-none')
        const renderScroll = flat?.props.renderScrollComponent
        assert.ok(typeof renderScroll === 'function'); renderScroll({})
        const scroll = ios.find(({ type }) => type === 'KeyboardChatScrollView')
        assert.equal(scroll?.props.extraContentPadding, dockHeight)
        assert.equal(scroll?.props.offset, 34)
        assert.equal(scroll?.props.keyboardLiftBehavior, 'whenAtEnd')
        const measure = ios.find(({ type, props }) => type === 'View' && typeof props.onLayout === 'function')?.props.onLayout
        assert.ok(typeof measure === 'function')
        measure({ nativeEvent: { layout: { height: 94 } } })
        assert.equal(dockHeight.value, 94)
        measure({ nativeEvent: { layout: { height: 94 } } })
        assert.equal(dockHeight.value, 94)
        measure({ nativeEvent: { layout: { height: 190 } } })
        assert.equal(dockHeight.value, 190)
        measure({ nativeEvent: { layout: { height: 94 } } })
        assert.equal(dockHeight.value, 94)
        assert.equal(emptyPadding().paddingBottom, 94)
        keyboard.height.value = -168; keyboard.progress.value = 0.5
        assert.equal(emptyPadding().paddingBottom, 245, 'safe area compensation follows keyboard progress')
        keyboard.height.value = -336; keyboard.progress.value = 1
        assert.equal(844 - emptyPadding().paddingBottom, 448, 'the open keyboard and dock reserve H + K - S')
        const offsets: number[] = [], ref = flat?.props.ref
        assert.ok(ref && typeof ref === 'object' && 'current' in ref)
        ref.current = { scrollToOffset: ({ offset }: { offset: number }) => offsets.push(offset) }
        measure({ nativeEvent: { layout: { height: 190 } } })
        assert.equal(dockHeight.value, 190, 'attachment and notice growth use the measured complete dock')
        assert.equal(emptyPadding().paddingBottom, 492)
        assert.deepEqual(offsets, [], 'dock growth cannot itself force an old-message reader to the latest message')
        const insetChanged = scroll?.props.onContentInsetChange, scrolled = flat?.props.onScroll
        assert.ok(typeof insetChanged === 'function' && typeof scrolled === 'function')
        insetChanged({ top: 492, bottom: 96 })
        scrolled({ nativeEvent: { contentOffset: { y: 0 }, contentInset: { top: 492, bottom: 96 }, contentSize: { height: 2000 }, layoutMeasurement: { height: 844 } } })
        const jump = chat.render('default').find(({ props }) => props.accessibilityLabel === '回到底部')?.props.onPress
        assert.ok(typeof jump === 'function'); jump()
        assert.deepEqual(offsets, [-492], 'the inverted list jumps to the negative actual native top inset')
        for (const [nextTheme, nextGlass, blur, background] of [
          ['light', true, 'none', 'white'], ['dark', true, 'none', 'black'],
          ['light', false, 'none', 'white'], ['dark', false, 'none', 'black'],
        ] satisfies [string, boolean, string, string][]) {
          theme = nextTheme; glass = nextGlass
          const elements = chat.render('default'), options = elements.find(({ type }) => type === 'Screen')?.props.options
          assert.ok(options && typeof options === 'object' && 'headerTransparent' in options && 'headerBlurEffect' in options && 'headerStyle' in options && 'headerTitle' in options)
          assert.equal(options.headerTransparent, true)
          assert.equal(options.headerBlurEffect, blur)
          assert.equal(JSON.stringify(options.headerStyle), JSON.stringify({ backgroundColor: background }))
          assert.ok(typeof options.headerTitle === 'function'); options.headerTitle()
          assert.equal(elements.some(({ type }) => type === 'ChatMaterial'), false, 'the native bar owns the title material')
        }
      } finally {
        for (const read of fileReads.values()) read.resolve(new Uint8Array([0]).buffer)
        await Promise.allSettled(mainSaves)
        chat.close(); layout.close(); bridge.close()
      }
    })
    await t.test('a cancelled safe-false refusal settles transport, allows its next input and never releases the cancellation', async () => {
      knownBots = [bot]
      await render().refresh(); await render().selectBot(bot)
      const before = messages.length, releases = inputReleases.length
      failMessage = true
      assert.equal(await render().sendMessage('cancelled head'), true)
      await settleComponent()
      const head = render().pendingMessages.find((item) => item.text === 'cancelled head')
      assert.ok(head); assert.equal(head.delivery, 'unknown')
      const inbox = () => inboxItems({ device: { ready: activeConnection, joins: [], pendingRecovery: null, rotateHint: false }, workspace: render() })
      failMessage = false
      try {
        await assert.rejects(render().releaseMessagePending(head.message_id))
        assert.equal(render().pendingMessages.find((item) => item.message_id === head.message_id)?.delivery, 'unknown')
        assert.ok(inbox().some((item) => item.key === `undelivered:${head.message_id}`), 'an unknown reminder cannot be dismissed')
        inputQuerySafe = false; inputQueryStage = 'refused'
        assert.equal(await render().sendMessage('following input'), true)
        await settleComponent()
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['cancelled head', 'following input'])
        assert.equal(render().pendingMessages.find((item) => item.message_id === head.message_id)?.delivery, 'accepted')
        await render().retryPendingMessage(head.message_id)
        assert.equal(inputReleases.length, releases, 'safe=false cannot release a cancelled input')
        assert.equal(messages.slice(before).filter((message) => message.message_id === head.message_id).length, 1)
        const saved = [...outboxItems.values()].find((item): item is OutboxRecord<'message'> => isMessageOutbox(item) && item.params.message_id === head.message_id)
        assert.ok(saved)
        inputQueryStage = 'unknown'
        await assert.rejects(render().releaseMessagePending(head.message_id))
        assert.ok(outboxItems.has(saved.key), 'a stale refused card cannot close after the fresh result becomes unknown')
        inputQueryStage = 'refused'; await render().queryInput(head)
        await memoryOutbox.put({ ...saved, rejected: '原输入内容不同', rejection_code: 'input_conflict' })
        await assert.rejects(render().releaseMessagePending(head.message_id))
        assert.ok(outboxItems.has(saved.key), 'a conflict keeps both copies for reconciliation')
        await memoryOutbox.put(saved); await render().queryInput(head)
        const rpcConnection = activeConnection, originalRpc = rpcConnection.rpc
        const closeCalls: string[] = []
        const history = { history_authority: 'authority', next_cursor: null, complete: true, events: [{ event_hash: 'a'.repeat(64), created_at_ms: 1, scope_id: head.conversation_id, kind: 'input_posted', speaker: { kind: 'human', device_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' }, body: { ...messages[before], type: 'input_posted' } }], coverage: { limit: 128, truncated: false, scope: 'workspace' } }
        rpcConnection.rpc = async (method, body = {}) => { closeCalls.push(method); return method === 'workspace_history' ? history : originalRpc(method, body) }
        let closeWork = Promise.resolve()
        const status = componentHarness('./workspace-chat.tsx', {
          '@vibeface/client/workspace/context.tsx': { useWorkspace: () => render() }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
          '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, action: () => Promise<void>) => { closeWork = action(); return closeWork } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
        })
        try {
          const sharedHistory = JSON.parse(JSON.stringify(await render().readWorkspaceHistory()))
          const cancelled = render().pendingMessages.find((item) => item.message_id === head.message_id)
          assert.ok(cancelled); assert.equal(cancelled.rejected, undefined); assert.ok(inbox().some((item) => item.key === `undelivered:${head.message_id}`))
          const close = status.render('WorkspaceInputStatus', { pending: cancelled }).find(({ type, props }) => type === 'Button' && componentText(props.children) === '关闭提醒')?.props.onClick
          assert.ok(typeof close === 'function')
          const writes = messages.length, commands = inputReleases.length, signatures = signed.length
          closeCalls.splice(0); close(); await closeWork; await settleComponent()
          assert.deepEqual(closeCalls, ['input_receipt'], 'closing only queries the terminal receipt and sends no write')
          assert.equal(render().pendingMessages.some((item) => item.message_id === head.message_id), false)
          assert.equal(inbox().some((item) => item.key === `undelivered:${head.message_id}`), false, 'the cancelled input leaves Inbox after closing its local reminder')
          assert.equal(outboxItems.has(saved.key), false)
          assert.deepEqual(JSON.parse(JSON.stringify(await render().readWorkspaceHistory())), sharedHistory, 'closing the local reminder preserves shared history')
          assert.equal(messages.length, writes); assert.equal(inputReleases.length, commands); assert.equal(signed.length, signatures)
        } finally { status.close(); rpcConnection.rpc = originalRpc }
      } finally { inputQueryStage = null; failMessage = false }
    })
    for (const outcome of ['not_saved', 'wrong_id', 'stored', 'responded', 'forward_ack']) await t.test(`Release ${outcome} only settles its exact proven unwritten command before advancing the lane`, async () => {
      knownBots = [bot]
      await render().refresh(); await render().selectBot(bot)
      inputControlEvent = 'e'.repeat(64)
      const before = messages.length, releases = inputReleases.length
      const firstText = `Release ${outcome} original`, nextText = `Release ${outcome} following`
      const response = deferred(), requested = deferred()
      void response.promise.catch(() => {})
      let recovery = Promise.resolve<PromiseSettledResult<void>[]>([])
      try {
        failMessage = true
        assert.equal(await render().sendMessage(firstText, [{ name: 'original.txt', arrayBuffer: async () => new Uint8Array([1, 2, 3]).buffer }]), true)
        await settleComponent(); failMessage = false
        const head = render().pendingMessages.find((item) => item.text === firstText)
        assert.ok(head)
        const receipt: InputReceipt = { message_id: head.message_id, target: head.target, stage: 'refused', actual_turn_id: null, reason: '原输入已拒绝', retry_safe: true, control_event: inputControlEvent }
        inputReceiptOverrides.set(head.message_id, receipt)
        await render().queryInput(head)
        inputReleaseReply = (body) => { requested.resolve(body); return response.promise }
        recovery = Promise.allSettled([render().retryPendingMessage(head.message_id)])
        const request = nodeSettings.record(JSON.parse(JSON.stringify(await requested.promise)))
        assert.equal(typeof request.command_id, 'string')
        const original = messages[before]
        const releasing = [...outboxItems.values()].find((item): item is OutboxRecord<'message'> => isMessageOutbox(item) && item.params.message_id === head.message_id)
        assert.ok(releasing); assert.equal(releasing.delivery, 'unknown'); assert.equal(releasing.release_expected_event, receipt.control_event); assert.equal(releasing.release_command_id, request.command_id)
        await assert.rejects(render().releaseMessagePending(head.message_id))
        assert.ok(outboxItems.has(releasing.key), 'an in-flight Release cannot be dismissed')
        assert.equal(await render().sendMessage(nextText), true)
        await settleComponent()
        const following = render().pendingMessages.find((item) => item.text === nextText)
        assert.ok(following)
        assert.deepEqual(messages.slice(before).map((message) => message.text), [firstText], 'the following input waits for the Release result')
        inputReceiptOverrides.set(head.message_id, { ...receipt, retry_safe: false })
        const queries = inputReceiptQueries.length
        const refusal = { accepted: false, code: 'workspace_command_not_saved', stored: false, command_id: request.command_id, reason: '任务版本已变化，操作未写入。' }
        if (outcome === 'responded') response.reject(Object.assign(new Error('owner response could not be confirmed'), { responded: true }))
        else if (outcome === 'forward_ack') response.reject(new Error('forward acknowledgement lost'))
        else response.resolve({ ...refusal, ...(outcome === 'wrong_id' ? { command_id: 'another-command' } : outcome === 'stored' ? { stored: true } : {}) })
        const [completed] = await recovery
        await settleComponent()
        assert.ok(inputReceiptQueries.slice(queries).some((query) => query.message_id === head.message_id), 'the Release result is followed by a fresh query of the original input')
        const checked = [...outboxItems.values()].find((item): item is OutboxRecord<'message'> => isMessageOutbox(item) && item.params.message_id === head.message_id)
        const control = [...outboxItems.values()].find((item): item is OutboxRecord<'workspace_command'> => isWorkspaceCommandOutbox(item) && item.params.command_id === request.command_id)
        assert.ok(checked); assert.deepEqual(checked.params, original, 'the original ID, signature, sequence and attachments are unchanged')
        assert.ok(control); assert.deepEqual(control.params, request, 'the original signed Release request remains durable')
        if (outcome === 'not_saved') {
          assert.equal(completed.status, 'rejected')
          if (completed.status === 'rejected') assert.match(String(completed.reason), /任务版本已变化/)
          assert.equal(checked.delivery, 'accepted'); assert.equal(checked.release_expected_event, undefined); assert.equal(checked.release_command_id, undefined)
          assert.equal(checked.receipt?.control_event, receipt.control_event); assert.equal(checked.receipt?.retry_safe, false)
          assert.deepEqual(control.command_rejection, refusal, 'only the exact trusted prewrite proof is durable')
        } else {
          assert.equal(completed.status, 'rejected')
          assert.equal(checked.delivery, 'unknown'); assert.equal(checked.release_expected_event, receipt.control_event); assert.equal(checked.release_command_id, request.command_id)
          assert.equal(control.command_rejection, undefined, 'an ambiguous or mismatched response is not an unwritten proof')
          await render().queryInput(head)
          await render().retryPendingMessage(head.message_id)
          await assert.rejects(render().releaseMessagePending(head.message_id))
          await settleComponent()
          const stillUnknown = render().pendingMessages.find((item) => item.message_id === head.message_id)
          assert.equal(stillUnknown?.delivery, 'unknown')
          assert.deepEqual(messages.slice(before).map((message) => message.text), [firstText], 'an old safe-false receipt cannot unblock an unconfirmed Release')
          inputReceiptOverrides.set(head.message_id, { ...receipt, retry_safe: false, control_event: 'f'.repeat(64) })
          await render().queryInput(head); await settleComponent()
        }
        assert.equal(inputReleases.length, releases + 1, 'queries never create another Release command')
        assert.deepEqual(messages.slice(before).map((message) => message.text), [firstText, nextText])
        assert.deepEqual(messages.slice(before).map((message) => message.message_id), [head.message_id, following.message_id])
        assert.equal(signed.filter((value) => value.includes('"type":"input_posted"') && value.includes(head.message_id)).length, 1, 'the original input is never re-signed')
        assert.equal(signed.filter((value) => value.includes('"type":"workspace_commanded"') && value.includes('"kind":"control_input"') && value.includes(String(request.command_id))).length, 1, 'the Release keeps its original signed identity')
      } finally {
        response.reject(new Error('fixture cleanup'))
        await recovery
        for (const receipt of inputReceiptOverrides.values()) {
          inputReceiptOverrides.set(receipt.message_id, { ...receipt, retry_safe: false, control_event: 'f'.repeat(64) })
          await render().queryInput(receipt)
        }
        await settleComponent()
        inputReleaseReply = null; inputReceiptOverrides.clear(); failMessage = false
      }
    })
    await t.test('a forged Release command identity is retained and pauses the whole ledger', async () => {
      const previous = [...outboxItems.values()].find((item): item is OutboxRecord<'workspace_command'> => item.method === 'workspace_command' && item.command_rejection !== undefined)
      assert.ok(previous?.command_rejection)
      const original = structuredClone(previous), snapshot = new Map(outboxItems)
      const secondId = previous.params.command_id === 'a'.repeat(64) ? 'b'.repeat(64) : 'a'.repeat(64)
      const second: OutboxItem = { ...previous, key: previous.key.replace(previous.params.command_id, secondId), params: { ...previous.params, command_id: secondId }, rejected: undefined, command_rejection: undefined }
      outboxItems.set(second.key, second)
      const bytes = JSON.stringify([...outboxItems]), sends = messages.length, releases = inputReleases.length, signatures = signed.length
      try {
        await reload(); await render().refresh()
        await assert.rejects(render().sendMessage('following forged Release'), /损坏/)
        assert.equal(render().canSaveMessages, false)
        assert.equal(JSON.stringify([...outboxItems]), bytes)
        assert.deepEqual(outboxItems.get(previous.key), original)
        assert.deepEqual([messages.length, inputReleases.length, signed.length], [sends, releases, signatures])
      } finally { outboxItems.clear(); for (const [key, item] of snapshot) outboxItems.set(key, item); await reload(); await render().refresh() }
    })
  } finally {
    hooks.remount()
  }
})
