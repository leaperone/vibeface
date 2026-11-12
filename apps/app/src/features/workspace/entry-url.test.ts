import { parseChanged } from '@vibeface/client/lib/progress.ts'
import * as workspaceHistory from '@vibeface/client/workspace/history.ts'
import { completeHistory } from '@vibeface/client/workspace/history-fixture.ts'
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import { entryAddress, entryJson, entryKey, messageOf, parseEntry, parseInfo, providerNames } from "@vibeface/client/lib/node.ts"
import * as nodeSettings from "@vibeface/client/lib/node.ts"
import type { OutboxItem } from '@vibeface/client/lib/outbox.ts'
import type { NodeSettingsResponse, ProviderReadiness, ProviderStatus, ReadyConnection } from "@vibeface/client/lib/node.ts"
import type { HistorySection, InputReceipt, InputOptions, InputTarget, ManagerJob, PendingMessage, WorkRecord, WorkspaceDomainState } from "@vibeface/client/workspace/types.ts"
import { cardState, nodeName, taskGroup } from "@vibeface/client/workspace/labels.ts"
import { botReadiness, inputText, parseArtifactPage, parseConversation, pendingChecks, parseNodes, parseCreateBot, parseBot, formatSize, attachmentName, signedOrder } from "@vibeface/client/workspace/model.ts"
import { botCopy, folderLabels, folderSetup, placeChanges, projectChoices, projectKey, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { answerLabel, attentionLabel, approvalTask, awaitsAnswer, declined, foldAnswers, ignoredAnswer, projectName, questionMeta, questionOpen, settledAnswer, sharedProject, questionParts } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { joinDestination } from '@vibeface/client/identity/switch-network.ts'
import { joinTicket } from '@vibeface/client/lib/transport.ts'
import { followUp, followUpCopy, promptSummary } from "@vibeface/client/workspace/labels.ts"
import { messageKey, replyStatusLabel, inputTargetKey, promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { deviceCopy, offlineCopy, setUpOnManager, waitingCopy } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread, messageTimes, taskMessage, unreadCounts } from "@vibeface/client/chat/timeline.ts"
import { accessOf } from "@vibeface/client/identity/access.ts"
import { networkKey, storageOf } from "@vibeface/client/workspace/storage.ts"
import { unreachable } from "@vibeface/client/workspace/home.ts"
import { primaryLane } from "@vibeface/client/identity/connection-lane.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"
import { inboxItems } from "@vibeface/client/workspace/inbox.ts"
import { notificationOpenings } from '../../../../../apps/mobile/src/platform/notification-opens.ts'
import type { NotificationDestination } from '@vibeface/client/lib/platform.ts'
import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'

test("bulk inbox actions re-arm focus after an intervening list update", () => {
  const source = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../../routes/_app/inbox.tsx"), "utf8")
  const start = source.indexOf("const action = async (kind")
  const action = source.slice(start, source.indexOf("\n  return <div", start))
  assert.ok(action.indexOf("held.current = { key: bulkFocusKey") >= 0)
  assert.ok(action.indexOf("held.current = { key: bulkFocusKey") < action.indexOf("setBusy(kind)"), "the action re-arms the sentinel before the list can change")
})

const context = {
  history_authority: "authority", history: completeHistory(),
  conversation: { conversation_id: "chat", title: "test", owner: "gateway", owner_epoch: 1 }, messages: [],
  works: [], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, questions: [], sessions: [],
}
const historyActions = { historyLoading: new Set<HistorySection>(), loadHistory: async (_section: HistorySection) => {}, cancelHistory(_section: HistorySection) {} }
test("attachment names are signed clean and files once each in content order", () => {
  assert.equal(attachmentName("../etc/pass\\wd\u0007\n.txt"), "..etcpasswd.txt")
  assert.equal(attachmentName("/\\"), "file", "a name with nothing left falls back")
  const long = attachmentName("名".repeat(100))
  assert.equal(new TextEncoder().encode(long).length, 255)
  assert.equal(attachmentName("a" + "名".repeat(85)), "a" + "名".repeat(84), "a character is never split")
  assert.equal(attachmentName("bad\ud800.txt"), "bad.txt", "a lone surrogate cannot reach the signed JSON")
  const files = [{ hash: "b", size: 1, name: "b.txt" }, { hash: "a", size: 1, name: "a.txt" }, { hash: "b", size: 1, name: "copy of b.txt" }]
  assert.deepEqual(signedOrder(files), [{ hash: "a", size: 1, name: "a.txt" }, { hash: "b", size: 1, name: "b.txt" }])
})
test("malformed conversation snapshots fail closed instead of authorizing writes", () => {
  assert.equal(parseConversation(context, "chat").conversation?.owner, "gateway")
  assert.throws(() => parseConversation(context, "other"))
  assert.throws(() => parseConversation({ ...context, works: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...context, local_node: undefined }, "chat"))
})
test("nodeName prefers rpc and directory names and never shows a peer id", () => {
  const id = "a".repeat(64)
  assert.equal(nodeName(id, { nodes: [{ device_id: id, name: "Mac Studio", local: false, online: true, revoked: false }] }), "Mac Studio")
  assert.equal(nodeName(id, { directory: { network_id: "n", epoch: 1, recovery_key: "n", heads: [], members: [{ device_id: id, kind: "node", name: "工作机", manage: false }], revoked: [], attesters: [], pending: null, stopped: [] } }), "工作机")
  assert.equal(nodeName(id), "另一台设备")
})

test("capabilities parse the actual provider name and reject malformed node lists", () => {
  const nodes = parseNodes({ nodes: [{ device_id: "n", name: "本机", local: true, online: true, revoked: false, capabilities: { manager: "cursor", manager_state: "unverified", roots: ["/project"] } }] })
  assert.equal(nodes[0]?.name, "本机")
  assert.equal(nodes[0]?.capabilities?.manager, "cursor")
  assert.throws(() => parseNodes({ nodes: [{ device_id: "n" }] }))
})
test("works, reply status and pending checks keep their exact shape", () => {
  const work = {
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    work_id: "work", trigger: "message:one", target_node: "worker", cwd: "/project", title: "检查项目", prompt: "检查项目", state: "running", summary: "进行中", outcome: null, result_text: "", failure_reason: null, resolved: { provider: "codex", model: null } }
  const scheduler = { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: "message:one", state: "queued", message_id: "one" }], unanswered: [], capabilities: { streaming: true, cancel: false, steer: false } }
  const message = { message_id: "reply", event_hash: "event", role: "assistant", text: "部分回复", created_at_ms: 1, in_reply_to: "one", status: "stopped" }
  const sent = { message_id: "one", event_hash: "sent", role: "user", text: "看附件", created_at_ms: 0, attachments: [{ hash: "file", size: 20, name: "报告.pdf" }] }
  const parsed = parseConversation({ ...context, works: [work], scheduler, messages: [message, sent] })
  assert.equal(parsed.works[0].state, "running")
  assert.deepEqual(parsed.works[0].resolved, { provider: "codex", model: null })
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, resolved: undefined }] }), "accepted work requires its frozen selection")
  const offline = parseNodes({ nodes: [{ device_id: "worker", name: "工作电脑", local: false, online: false, revoked: false }] })
  assert.equal(cardState(parsed.works[0], offline, { state: "running", outcome: null }), "awaiting_confirmation", "the last overview state cannot make an offline task look active")
  assert.equal(taskGroup(cardState(parsed.works[0], offline, { state: "running", outcome: null }), false), "attention")
  assert.equal(cardState(parsed.works[0], [{ ...offline[0], online: true }], { state: "running", outcome: null }), "running")
  assert.equal(cardState({ ...parsed.works[0], state: "unverified", outcome: "unverified" }, [], { state: "unverified", outcome: "unverified" }), "awaiting_confirmation")
  assert.equal(cardState({ ...parsed.works[0], outcome: "failed" }, offline, { state: "finished", outcome: "failed" }), "failed")
  assert.equal(cardState({ ...parsed.works[0], state: "dispatch_waiting" }, offline, { state: "dispatch_waiting", outcome: null }), "dispatch_waiting")
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "waiting_approval" }] }), "approval states are gone")
  assert.equal(parseConversation({ ...context, works: [{ ...work, title: undefined }] }).works[0].title, "", "a dispatch row without a title still parses")
  assert.equal(parsed.messages[0].status, "stopped")
  assert.equal(parsed.messages[0].in_reply_to, "one")
  assert.deepEqual(parsed.messages[1].attachments, [{ hash: "file", size: 20, name: "报告.pdf" }])
  assert.throws(() => parseConversation({ ...context, messages: [{ ...sent, attachments: [{ hash: "file", size: 20 }] }] }), "an attachment without a name fails closed")
  assert.equal(parseConversation(context).scheduler, null)
  for (const outcome of ["completed", "failed", "interrupted", "rejected", "unverified"]) {
    assert.equal(parseConversation({ ...context, works: [{ ...work, state: "finished", outcome }] }).works[0].outcome, outcome)
  }
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "finished", outcome: "abandoned" }] }))
  assert.equal(parseConversation({ ...context, messages: [{ ...message, status: "failed", failure: "quota" }] }).messages[0].failure, "quota")
  assert.equal(parseConversation({ ...context, works: [{ ...work, failure: "execution_disabled" }] }).works[0].failure, "execution_disabled")
  assert.throws(() => parseConversation({ ...context, messages: [{ ...message, failure: "usage limit" }] }), "an unknown failure kind fails closed")
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: undefined }] }), "a DAG work always has a state")
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "waiting" }] }))
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "waiting_for_device" }] }), "undelivered dispatches arrive as deliveries")
  assert.throws(() => parseConversation({ ...context, messages: [{ ...message, status: "cancelled" }] }))
  const check = { check_id: "manager:attempt", conversation_id: "chat", trigger: "message:one", attempt_id: "attempt", detail: "进程还在运行", process_groups: [42], created_at_ms: 1 }
  assert.deepEqual(parseConversation({ ...context, checks: [check] }).checks, [check])
  assert.throws(() => parseConversation({ ...context, checks: [{ ...check, attempt_id: undefined }] }))
  assert.throws(() => parseConversation({ ...context, checks: undefined }))
  const unverified = parseConversation({ ...context, works: [{ ...work, state: "unverified", outcome: "unverified", failure_reason: "无法证明结果" }], checks: [check] })
  assert.deepEqual(pendingChecks(unverified, [], "chat"), [
    { kind: "work", check_id: "work:work", detail: "无法证明结果", work_id: "work", target_node: "worker", conversation_id: "chat", title: "检查项目", reason: "unverified" },
    { kind: "process", check_id: "manager:attempt", detail: "进程还在运行" },
  ], "an unverified work is its own card; a coordination check is confirmed stopped")
  const delivered = parseConversation({ ...context, works: [work], deliveries: [
    { work_id: "new", target_node: "other", state: "dispatch_rejected", kind: "codex", title: "新任务", cwd: "/p", reason: "project unavailable" },
    { work_id: "work", target_node: "worker", state: "stop_timed_out" },
    { work_id: "lost", target_node: "gone", state: "dispatch_unverified", kind: "task", title: "旧任务", cwd: "/p", reason: "设备已移除，结果无法确认。" },
  ] })
  assert.equal(delivered.works[0].stop, "stop_timed_out")
  assert.equal(delivered.works[2].state, "dispatch_unverified")
  assert.deepEqual(pendingChecks(delivered, [], "chat"), [], "an unknown dispatch has nothing in the DAG to settle")
  assert.deepEqual(delivered.works[1], { work_id: "new", trigger: "", target_node: "other", cwd: "/p", title: "新任务", prompt: "", state: "dispatch_rejected", summary: "", outcome: null, result_text: "", failure_reason: "project unavailable" })
  assert.throws(() => parseConversation({ ...context, deliveries: [{ work_id: "w", target_node: "n", state: "waiting_for_device" }] }))
  assert.throws(() => parseConversation({ ...context, scheduler: { ...scheduler, jobs: [{ ...scheduler.jobs[0], state: 1 }] } }))
  assert.equal(parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: "message:two", owner: "node", conversation_id: "c", state: "pending_delivery", message_id: "two" }], unanswered: [] } }).scheduler?.jobs[0].state, "pending_delivery")
  assert.throws(() => parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [] } }), "the unanswered list is always sent")
})
test("artifact pages stay bound to their original reference and advance without skipping the end", () => {
  const page = { blob_hash: "result", text: "first", offset: 0, next_offset: 5, size: 10 }
  assert.deepEqual(parseArtifactPage(page, "result", 0), { text: "first", offset: 0, next_offset: 5, size: 10 })
  assert.equal(parseArtifactPage({ ...page, offset: 5, next_offset: null }, "result", 5).next_offset, null)
  for (const invalid of [{ ...page, blob_hash: "other" }, { ...page, offset: 1 }, { ...page, next_offset: 0 }, { ...page, next_offset: 11 }, { ...page, next_offset: undefined }, { ...page, text: "" }, { ...page, next_offset: null }, { ...page, next_offset: 5, text: "x" }, { ...page, size: 0, next_offset: null }]) assert.throws(() => parseArtifactPage(invalid, "result", 0))
  assert.deepEqual(parseArtifactPage({ blob_hash: "result", text: "中", offset: 0, next_offset: null, size: 3 }, "result", 0), { text: "中", offset: 0, next_offset: null, size: 3 })
})

test("creation fields use the server UTF-8 byte limits and reject whitespace", () => {
  assert.equal(inputText("  名称  ", "机器人名称", 512), "名称")
  assert.equal(inputText("中".repeat(170), "机器人名称", 512), "中".repeat(170))
  assert.throws(() => inputText("中".repeat(171), "机器人名称", 512), /512/)
  assert.throws(() => inputText("   ", "职责说明", 8192), /不能为空/)
  assert.equal(inputText("a".repeat(8192), "职责说明", 8192).length, 8192)
  assert.throws(() => inputText("中".repeat(2731), "职责说明", 8192), /8192/)
})

test("workspace preserves bot and message requests while routing native decisions to their exact work", async (t) => {
  const hooks = hookHarness(), { react, values, effects, afterRender } = hooks
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
  const signed: string[] = [], attestation = { device_id: "client", signature: "signature" }
  const botBody = (pending: unknown) => ({ ...JSON.parse(JSON.stringify(parseCreateBot(pending))), scheduler_node: "gateway", owner_attestation: attestation, attestation })
  const outboxItems = new Map<string, OutboxItem>()
  const memoryOutbox = {
    all: async () => [...outboxItems.values()].map((item) => structuredClone(item)),
    put: async (item: OutboxItem) => { outboxItems.set(item.key, structuredClone(item)) },
    delete: async (key: string) => { outboxItems.delete(key) },
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
    secure: { attestationPayload: (network: string, scope: string, body: string) => { const event = JSON.parse(body); assert.equal(typeof event.type, "string"); assert.equal(event.attestation, undefined); return `${network}|${scope}|${body}` } },
    transport: { signClient: async (domain: string, payload: string) => { signed.push(`${domain}|${payload}`); return "signature" } },
    storage: browser.localStorage, outbox: memoryOutbox, self: "这个浏览器",
    inputLock: <T,>(name: string, save: () => Promise<T>) => globalThis.navigator.locks.request(name, save),
    notifier: { name: "浏览器提醒", permission: async () => "default", request: async () => "default", show() {} },
    onForeground: () => () => {}, foreground: () => true,
  }
  const bot = { bot_id: "bot", conversation_id: "chat", name: "Bot", instructions: "处理项目", models: {}, skills: null, avatar: null, avatar_version: null, event_hash: "bot-event", last_message: null, busy: null }
  const work: WorkRecord = {
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    work_id: "work", trigger: "t", target_node: "gateway", cwd: "/project", title: "工作", prompt: "工作", state: "running", resolved: { provider: "codex", model: null }, summary: "", outcome: null, result_text: "", failure_reason: null }
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
  const questionRow = { question_id: "q1", conversation_id: "chat" as string | null, bot: { bot_id: "bot", name: "Bot" } as { bot_id: string; name: string } | null, severity: "attention", text: "要不要继续", choices: [{ label: "继续" }, { label: "停下" }], about: { node: "worker", agent: "codex", session_id: "thread", cwd: "/project", summary: "改登录" }, author: "gateway", created_at_ms: 1, answer: null }
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
    get generation() { return baseGeneration }, entry: { kind: "gateway", device_id: "gateway", address: "https://node.example" },
    principal: { network_id: "network", device_id: "client", epoch: 1, can_manage: true, source: "membership" },
    info: { device_id: "gateway", name: "gateway", version: "test", initialized: true, network_id: "network" },
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
      if (method === "conversation") return conversationHold ? conversationHold.promise : { ...context, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, messages: conversationMessages?.[String(body.conversation_id)] ?? transcript, works: [work] }
      if (method === "create_bot") {
        creations.push(JSON.parse(JSON.stringify(body)))
        assert.deepEqual(botBody(JSON.parse(stored.get("vibeface.pending-bot.client.network") ?? "null")), creations.at(-1))
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
    for (const effect of effects.values()) effect.cleanup?.()
    effects.clear(); values.length = 0; afterRender.length = 0
    render(); render().setView("chat"); render()
    // The outbox read that unlocks writing is asynchronous; the next render starts the first refresh.
    await new Promise(setImmediate); render()
    await new Promise(setImmediate)
  }
  try {
    render(); render().setView("chat"); render()
    // The outbox read that unlocks writing is asynchronous; the next render starts the first refresh.
    await new Promise(setImmediate); render()
    await new Promise(setImmediate)
    let state = render()
    assert.equal(state.writable, true)
    await state.refresh()
    state = render()
    assert.equal(state.selectedBot?.bot_id, "bot")
    await t.test('malformed signed Unknown inputs survive reload and recovery attempts without rewriting or resubmission', async () => {
      const originalOutbox = platform.outbox
      const mutations: string[] = []
      platform.outbox = { all: originalOutbox.all, put: async (item) => { mutations.push('put'); await originalOutbox.put(item) }, delete: async (key) => { mutations.push('delete'); await originalOutbox.delete(key) } }
      try {
        for (const report_results of [undefined, null, 'false', 0]) {
          const invalid: OutboxItem = { key: 'network.client:message:legacy', method: 'message', created_at_ms: 1, delivery: 'unknown',
            params: { conversation_id: 'chat', message_id: 'legacy', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, text: '原签名请求', ...(report_results === undefined ? {} : { report_results }), attestation: { device_id: 'client', signature: 'must-stay-original' } },
            uploads: [{ hash: 'a'.repeat(64), size: 3, name: 'original.txt', data: 'YWJj' }],
          }
          await originalOutbox.put(invalid)
          const bytes = JSON.stringify(await originalOutbox.all()), signatures = signed.length, submissions = messages.length, queries = inputReceiptQueries.length
          mutations.length = 0
          await reload()
          assert.match(render().error?.text ?? '', /report_results/)
          assert.equal(render().canSaveMessages, false)
          await render().refresh()
          await new Promise(setImmediate)
          await assert.rejects(render().retryPendingMessage('legacy'), /report_results/)
          await assert.rejects(render().releaseMessagePending('legacy'), /report_results/)
          assert.equal(JSON.stringify(await originalOutbox.all()), bytes, 'raw ID, signature, delivery and upload bytes survive every failed boundary')
          assert.deepEqual(mutations, [], 'invalid durable wire is neither pruned nor rewritten')
          assert.deepEqual([signed.length, messages.length, inputReceiptQueries.length], [signatures, submissions, queries], 'no signing, write or input reconciliation is attempted')
          await originalOutbox.delete(invalid.key)
        }
      } finally {
        platform.outbox = originalOutbox
        await originalOutbox.delete('network.client:message:legacy')
        await reload()
        await render().refresh()
      }
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
        await new Promise(setImmediate)
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
        await render().loadHistory('works')
        assert.equal(render().data?.works.find((item) => item.work_id === 'older-work')?.state, 'running')
        held = deferred()
        const oldGeneration = render().loadHistory('messages')
        baseGeneration += 1
        render()
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('wrong-generation', 0)], next_cursor: null, complete: true })
        await oldGeneration
        await new Promise(setImmediate); render()
        await new Promise(setImmediate)
        await render().refresh()
        assert.equal(render().data?.history_authority, 'fork-authority', 'same-authority reconnect still resets retained history')
        assert.equal(render().data?.works.some((item) => item.work_id === 'older-work'), false, 'a missed terminal Work push outside the latest window cannot remain visibly running')
        assert.equal(render().data?.messages.some((item) => item.message_id === 'wrong-generation'), false)

        held = deferred()
        const oldNetwork = render().loadHistory('messages')
        const nextConnection: ReadyConnection = { ...connection, generation: 2, principal: { ...connection.principal, network_id: 'another-network' }, isCurrent: () => activeConnection === nextConnection }
        activeConnection = nextConnection
        render()
        held.resolve({ conversation_id: 'chat', section: 'messages', history_authority: authority, items: [row('wrong-network', 0)], next_cursor: null, complete: true })
        await oldNetwork
        assert.equal(render().data, null)
        activeConnection = connection
        render()
        await new Promise(setImmediate)
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
    const retryMessage = { message_id: "retry-1", event_hash: "retry-event", role: "user", text: "改 README", created_at_ms: 1 }
    const retryScheduler = { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: "message:retry-1", state: "failed", message_id: "retry-1", failure: { kind: "rejected", detail: "没有执行" } }], unanswered: [] }
    transcript = [retryMessage]
    scheduler = retryScheduler
    const heldRefresh = deferred()
    conversationHold = heldRefresh
    const refreshing = render().refresh()
    await new Promise(setImmediate)
    const retrying = render().retryReply("retry-1")
    await new Promise(setImmediate)
    assert.equal(messages.length, 0, "retry waits for an in-flight full read before sending")
    heldRefresh.resolve({ ...context, messages: [retryMessage], scheduler: retryScheduler })
    await refreshing
    assert.equal(await retrying, true)
    await new Promise(setImmediate)
    assert.equal(messages.length, 1)
    assert.match(String(messages[0]?.text), /上次没回完/)
    conversationHold = null
    await render().refresh()
    messages.splice(0)
    conversationHold = null
    scheduler = null
    transcript = []
    stored.set("vibeface.ignored-inbox.client.network", JSON.stringify(["from-other-tab"]))
    unreadNotifications = [{ notification_id: "local-note", kind: "manager_attention", text: "看看进展", conversation_id: "chat", target: { kind: "bot_main", bot_id: "bot" } }]
    render().setView("tasks")
    await render().refresh()
    render().ignoreNotifications(["local-note"])
    assert.deepEqual(JSON.parse(stored.get("vibeface.ignored-inbox.client.network") ?? "null"), ["from-other-tab", "local-note"], "ignoring merges the latest local storage from another tab")
    unreadNotifications = []
    render().setView("chat")
    failMessage = true
    assert.equal(await render().sendMessage("work", [{ name: "retry.txt", arrayBuffer: async () => new Uint8Array([7, 8, 9]).buffer }]), true)
    await new Promise(setImmediate)
    state = render()
    const saved = state.pendingMessages.find((item) => item.text === "work")
    assert.ok(saved)
    const pending = JSON.parse(JSON.stringify(saved))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, target: { kind: "bot_main", bot_id: "bot" }, intent: "continue", sequence: pending.sequence, text: "work", attachments: pending.attachments, report_results: true, attestation })
    assert.equal(signed.at(-1), `attestation|network|chat|${JSON.stringify({ type: "input_posted", message_id: pending.message_id, target: pending.target, intent: pending.intent, sequence: pending.sequence, text: "work", attachments: pending.attachments, report_results: true })}`)
    assert.equal(render().canSaveMessages, true, "an unknown prior input leaves local saving available")
    failMessage = false
    connection.entry.device_id = "other-gateway"
    const inputUiErrors: string[] = []
    let inputWork = Promise.resolve()
    const inputUi = componentHarness('./workspace-chat.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => render() }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, action: () => Promise<void>) => { inputWork = action().catch((cause) => { inputUiErrors.push(String(cause)) }); return inputWork } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
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
      const retained = [...outboxItems.values()].find((item) => item.method === 'message' && item.params.message_id === pending.message_id)
      assert.ok(retained)
      assert.deepEqual(retained.params, messages[0], 'original ID, signature, sequence and attachments stay unchanged')
      assert.equal(signed.filter((value) => value.includes('"type":"input_posted"') && value.includes(pending.message_id)).length, 1)
      assert.equal(inputReleases.length, 1)
      const retainedControl = [...outboxItems.values()].find((item) => item.method === 'workspace_command')
      assert.ok(retainedControl); assert.deepEqual(retainedControl.params, inputReleases[0], 'the unknown Release retains its original signed control request')
      inputControlEvent = 'd'.repeat(64); inputQuerySafe = false; inputQueryStage = 'entry_accepted'
      await pressInput('查询结果')
      const settled = render().pendingMessages.find((item) => item.message_id === pending.message_id)
      assert.ok(!settled || settled.delivery === 'accepted')
      if (settled) assert.equal(inputButton('重试原输入'), undefined)
    } finally { inputUi.close(); inputQuerySafe = false; inputQueryStage = null }
    connection.entry.device_id = "gateway"
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
    assert.equal(signed.at(-1), 'attestation|network|chat|{"type":"message_posted","message_id":"a-q1","role":"user","text":"继续","report_results":true,"answers":"q1"}', "the answer is signed as the chat message the Node writes")
    const signatures = signed.length
    await assert.rejects(render().answer(question, "停下"), /还没确认送达/)
    assert.equal(answers.length, 1, "another answer waits until the first is settled")
    answerOutcome = "answered"
    assert.deepEqual(JSON.parse(JSON.stringify(await render().answer(question, "继续"))), { status: "answered", text: "继续" })
    assert.deepEqual(answers, [{ question_id: "q1", text: "继续", attestation }, { question_id: "q1", text: "继续", attestation }], "trying again sends the stored request")
    assert.equal(signed.length, signatures, "nothing is signed anew")
    assert.deepEqual(render().pendingAnswers, [])
    assert.equal(render().answered.q1, "继续", "the answer shows before the next read")
    answerOutcome = "already_answered"
    assert.equal((await render().answer(other, "继续")).text, "停下", "the answer that stood first is the one shown")
    assert.equal(signed.at(-1), 'attestation|network|other|{"type":"message_posted","message_id":"a-s1","role":"user","text":"继续","report_results":true,"answers":"s1"}', "an answer is signed for its question's chat")
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
    for (let turn = 0; turn < 10; turn++) await new Promise(setImmediate)
    assert.deepEqual(answers.slice(beforeSettle).map((item) => item.question_id), ["q2"], "an answer whose question is still open is sent again; one already answered is not")
    assert.deepEqual(render().pendingAnswers, [])
    await render().resolveCheck({ check_id: "work:work", kind: "work", detail: "", work_id: "work", target_node: "worker", conversation_id: "chat", title: "", reason: "offline" }, "completed", " 设备上已看到结果 ")
    assert.deepEqual(controls.pop(), { method: "resolve", conversation_id: "chat", check_id: "work:work", outcome: "completed", note: "设备上已看到结果", attestation })
    assert.equal(signed.at(-1), 'attestation|network|chat|{"type":"work_finished","work_id":"work","outcome":"completed","result_text":"设备上已看到结果"}', "the choice is signed as the settlement the Node writes")
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
    await assert.rejects(render().createBot({ name: "n".repeat(512), instructions: "a".repeat(8192), project: { target_node: "executor", cwd: "/" + "p".repeat(4095) } }), /太长/)
    assert.equal(render().pendingBot, null)
    await assert.rejects(render().createBot({ name: "项目 Bot", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /connection lost/)
    const originalCreation = creations[0]
    assert.equal(typeof originalCreation.bot_id, "string")
    assert.notEqual(originalCreation.bot_id, originalCreation.conversation_id)
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    await reload()
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    await assert.rejects(render().createBot({ name: "改名", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /内容已保留/)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "项目 Bot", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /project unavailable/)
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    assert.equal(render().pendingBot?.submission?.state, "unconfirmed")
    createMode = "success"
    await render().createBot({ name: "项目 Bot", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } })
    assert.deepEqual(creations.slice(1), [originalCreation, originalCreation, originalCreation], "the reload resubmits the unconfirmed creation from the outbox")
    assert.equal(render().pendingBot, null)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "invalid project", instructions: "", project: { target_node: "executor", cwd: "/unavailable" } }), /确认没建成/)
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
        await new Promise(setImmediate)
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
        await new Promise(setImmediate)
        assert.equal(render().canSaveMessages, false, "the new connection must finish its own authorized read")
        await assert.rejects(render().sendMessage("new unread connection"), /先读取|补读/)
        assert.equal(messages.length, before + 1)
        if (outcome === "success") reading.resolve({ ...context, scheduler: null, works: [work] })
        else reading.reject(new Error("new read unavailable"))
        await new Promise(setImmediate)
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
        await new Promise(setImmediate)
        assert.equal(messages.length, before + 1, "reconnect queries the original input without replaying its write")
        transcript = [...transcript, posted(original.message_id)]
        await render().queryInput(original)
        await render().refresh()
        assert.equal(render().pendingMessages.some((item) => item.message_id === original.message_id), false)
      }
      messageHold = null
      conversationHold = deferred()
      const next: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === next }
      activeConnection = next; render(); await new Promise(setImmediate)
      conversationHold.resolve({ ...context, scheduler: null, works: [work] })
      await new Promise(setImmediate)
      assert.equal(render().writable, true)
      conversationHold = null
      await render().createBot({ name: "unrelated new bot", instructions: "" })
      assert.equal(render().selectedBot?.name, "unrelated new bot")
      const currentId = render().conversationId, ackBefore = acknowledgements.length
      unreadNotifications = [{ notification_id: "after-reconnect", kind: "result", text: "unread", conversation_id: currentId, target: { kind: "bot_main", bot_id: render().selectedBot!.bot_id } }]
      conversationHold = deferred()
      const restored: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === restored }
      activeConnection = restored; render(); await new Promise(setImmediate)
      conversationHold.resolve({ ...context, conversation: { ...context.conversation, conversation_id: currentId }, scheduler: null })
      await new Promise(setImmediate)
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
      await new Promise(setImmediate)
      const saved = render().pendingMessages
      assert.deepEqual(saved.map((item) => item.text), ['input one', 'input two', 'input three'])
      assert.equal(new Set(saved.map((item) => item.message_id)).size, 3)
      assert.equal(messages.length, before + 1, 'only the first write is in flight for this target')
      const read = deferred(); conversationHold = read
      const refreshing = render().refresh()
      await new Promise(setImmediate)
      messageHold = null
      transcript = [...transcript, posted(saved[0].message_id)]
      firstRpc.resolve({ accepted: true, state: 'steering', message_id: saved[0].message_id })
      await new Promise(setImmediate)
      assert.deepEqual(messages.slice(before).map((message) => message.text), ['input one', 'input two', 'input three'])
      assert.deepEqual(messages.slice(before).map((message) => message.message_id), saved.map((item) => item.message_id))
      read.resolve({ ...context, messages: transcript, scheduler: null, works: [work] })
      await refreshing
      conversationHold = null
      await render().refresh()
      assert.equal(render().pendingMessages.length, 0)
    })

    await t.test('two tabs preserve target order while the earlier input is still signing', async () => {
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
        if (payload.includes('"text":"first tab input"')) { signing = true; await gate.promise }
        return sign(domain, payload)
      }
      const previousBots = knownBots, previousTranscript = transcript, previousMessages = messages.length
      const previousOutbox = new Map(outboxItems)
      const independentBot = { ...bot, bot_id: 'other-tab-bot', conversation_id: 'other-tab-chat' }
      let first: Promise<boolean> | undefined, second: Promise<boolean> | undefined, sameTabSecond: Promise<boolean> | undefined, independent: Promise<boolean> | undefined
      try {
        knownBots = [bot, independentBot]
        await render().refresh()
        other(); other().setView('chat'); other()
        await new Promise(setImmediate); other(); await new Promise(setImmediate)
        assert.equal(other().writable, true)
        const before = messages.length
        first = other().sendMessage('first tab input')
        await new Promise(setImmediate)
        assert.equal(signing, true, 'the follower reserved its sequence and is waiting on signing')
        independent = render().sendMessage('independent tab input', [], [], { target: { kind: 'bot_main', bot_id: independentBot.bot_id }, conversation_id: independentBot.conversation_id })
        await new Promise(setImmediate)
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['independent tab input'], 'a different target must remain independent of the held signature')
        sameTabSecond = other().sendMessage('earlier same tab second input')
        await new Promise(setImmediate)
        second = render().sendMessage('second tab input')
        await new Promise(setImmediate)
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['independent tab input'], 'a later tab cannot submit ahead of the input still being signed')
        gate.resolve(null)
        assert.deepEqual(await Promise.all([first, sameTabSecond, second, independent]), [true, true, true, true])
        await render().refresh(); await new Promise(setImmediate)
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['independent tab input', 'first tab input', 'earlier same tab second input', 'second tab input'])
        assert.equal(Number(messages[before + 2].sequence), Number(messages[before + 1].sequence) + 1)
        assert.equal(Number(messages[before + 3].sequence), Number(messages[before + 2].sequence) + 1)
      } finally {
        gate.resolve(null)
        await Promise.allSettled([first, sameTabSecond, second, independent])
        platform.transport.signClient = sign
        knownBots = previousBots
        for (const effect of otherHooks.effects.values()) effect.cleanup?.()
        await render().refresh(); await new Promise(setImmediate)
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
      await new Promise(setImmediate)
      const saved = render().pendingMessages
      assert.deepEqual(saved.map((item) => item.text), ['attachment first', 'text second', 'text third'])
      assert.equal(saved[0].attachments?.[0]?.name, 'notes.txt')
      assert.equal(messages.length, before, 'the later text cannot pass the earlier attachment')
      await reload()
      assert.deepEqual(render().pendingMessages.map((item) => item.message_id), saved.map((item) => item.message_id), 'reload retains each original input identity')
      uploadHold = null; uploading.resolve({ complete: true })
      await new Promise(setImmediate)
      assert.deepEqual(messages.slice(before).map((message) => message.text), ['attachment first', 'text second', 'text third'])
      await render().refresh()
      assert.equal(render().pendingMessages.length, 0)
    })

    await t.test('a locally created channel task receives follow-ups before its create command is acknowledged', async () => {
      const current = activeConnection, originalRpc = current.rpc
      const creating = deferred()
      const commands: Record<string, unknown>[] = []
      const saved: PendingMessage[] = []
      const before = messages.length
      let domain: WorkspaceDomainState = {
        channels: [{ channel_id: 'channel', title: '频道', purpose: '', members: ['bot'], coordinator_bot: 'bot', coordinator_node: 'gateway', owner_epoch: 1, revision: 1, event_hash: 'channel-event', created_at_ms: 1 }],
        tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false },
      }
      current.rpc = async (method, body = {}) => {
        if (method === 'workspace_state') return domain
        if (method === 'workspace_command') {
          const command = workspaceModel.parseWorkspaceCommand(body.command)
          assert.ok(command.kind === 'create_task')
          assert.ok(command.expected_result.trim(), 'Node valid_text rejects an empty expected result')
          assert.ok(new TextEncoder().encode(command.expected_result).length <= 16384)
          commands.push(JSON.parse(JSON.stringify(body))); return creating.promise
        }
        if (method === 'workspace_history') return {
          events: messages.slice(before).map((message, index) => ({ event_hash: String(message.message_id), created_at_ms: index + 1, scope_id: 'chat', kind: 'input_posted', speaker: { kind: 'human', device_id: 'client' }, body: { ...message, type: 'input_posted' } })),
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
        const target = first.target
        const plainTarget = JSON.parse(JSON.stringify(target))
        assert.deepEqual(await Promise.all([
          render().sendMessage('channel two', [], undefined, { target }),
          render().sendMessage('channel three', [], undefined, { target, expected_revision: first.expected_revision, conversation_id: first.conversation_id }),
        ]), [true, true])
        await new Promise(setImmediate)
        assert.equal(commands.length, 1, 'only the original creation is submitted while its acknowledgement is held')
        assert.equal(messages.length, before, 'input delivery waits behind its real task creation')
        assert.deepEqual(JSON.parse(JSON.stringify(render().pendingMessages.map((input) => input.target))), [plainTarget, plainTarget, plainTarget])
        const command = workspaceModel.parseWorkspaceCommand(commands[0].command)
        assert.ok(command.kind === 'create_task')
        assert.equal(command.task_id, target.task_id)
        const event = 'c'.repeat(64)
        domain = { ...domain, tasks: [{ ...command, root_task_id: command.task_id, revision: 1, action: 'continue', request_event: event, event_hash: event, created_at_ms: 1, updated_at_ms: 1, state: 'not_started' }] }
        creating.resolve({ ...domain, command_id: commands[0].command_id, event_hash: event, duplicate: false })
        await new Promise(setImmediate)
        assert.deepEqual(messages.slice(before).map((message) => message.text), ['channel one', 'channel two', 'channel three'])
        assert.deepEqual(messages.slice(before).map((message) => message.target), [plainTarget, plainTarget, plainTarget])
        await render().readWorkspaceHistory({ task_id: target.task_id })
        assert.equal(render().pendingMessages.length, 0)
      } finally { current.rpc = originalRpc }
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
          if (method === 'workspace_history') return { events: messages.slice(before).map((message, index) => ({ event_hash: String(message.message_id), created_at_ms: index + 1, scope_id: 'chat', kind: 'input_posted', speaker: { kind: 'human', device_id: 'client' }, body: { ...message, type: 'input_posted' } })), coverage: { limit: 128, truncated: false, scope: 'task' } }
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
        if (method === 'workspace_history') return { events: [{ event_hash: 'attachment-event', created_at_ms: 1, scope_id: otherBot.conversation_id, kind: 'input_posted', speaker: { kind: 'human', device_id: 'client' }, body: { type: 'input_posted', report_results: true, message_id: 'attachment-input', target, intent: 'continue', sequence: 1, expected_revision: 1, text: '任务附件', attachments: [{ hash, size: bytes.length, name: 'scope.txt' }] } }], coverage: { limit: 128, truncated: false, scope: target.kind === 'task' ? 'task' : 'channel' } }
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
          const chat = componentHarness('../chat/elements/chat.tsx', { '@vibeface/client/chat/timeline.ts': { messageTimes: () => new Map() }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } })
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
      knownNodes = [{ ...parityNode, device_id: 'gateway' }]
      questionRows = []
      conversationMessages = Object.fromEntries([botA, botB].map((item) => [item.conversation_id, [{ message_id: `${item.bot_id}-message`, event_hash: `${item.bot_id}-event`, role: 'user', text: `${item.name} 的消息`, created_at_ms: 1 }]]))
      render().setView('other')
      await render().selectBot(botA)
      let shared = render()
      const device = { get ready() { return activeConnection }, phase: 'ready', link: { kind: 'online' }, entries: [], directory: null, identity: { deviceId: 'client' }, join() {}, cancelJoin() {}, leave() {} }
      const fileReads = new Map<string, ReturnType<typeof deferred>>()
      class SelectedFile { readonly uri: string; constructor(uri: string) { this.uri = uri } arrayBuffer() { const read = fileReads.get(this.uri); assert.ok(read); return read.promise } }
      const bridge = componentHarness('../../../../../apps/mobile/src/data/live.tsx', {
        'expo-file-system': { File: SelectedFile },
        '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/identity/access.ts': { accessOf },
        '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/lib/node.ts': { entryAddress },
        '@vibeface/client/chat/protocol.ts': { promptIntent, botReach, composerHint }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, messageTimes, taskMessage, unreadCounts },
        '@vibeface/client/workspace/bot-settings.ts': { botCopy }, '@vibeface/client/workspace/inbox.ts': { inboxItems }, '@vibeface/client/workspace/labels.ts': taskLabels,
        '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers }, '@vibeface/client/workspace/storage.ts': { storageOf }, '@vibeface/client/workspace/types.ts': { isSessionNotification: () => false },
        '@/platform': { mobilePlatform: { storage: browser.localStorage } }, './context': { WorkspaceProvider: 'NativeWorkspace' },
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
      mobile()
      let routine: string | undefined
      const nativeNavigations: string[] = []
      const platform = { OS: 'android' }, insets = { bottom: 0 }, keyboard = { height: { value: 0 }, progress: { value: 0 } }
      let glass = false, theme = 'light'
      let dockHeight: { value: number } | undefined, emptyPadding = () => ({ paddingBottom: 0 })
      const chat = componentHarness('../../../../../apps/mobile/app/b/[botId].tsx', {
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
        'expo-router': { Stack: stack, usePathname: () => '/' }, 'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) }, 'expo-linking': { useLinkingURL: () => null }, '@/data/live': { LiveWorkspace: 'LiveWorkspace', useLive: () => shared, useWaiting: () => 0 },
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
            const streaming = mobile([{ kind: 'manager', key: 'host-progress', conversation_id: 'chat', trigger: host.trigger, source_node: 'gateway', text: '部分回复', sequence: 1, items: [], needsRefresh: false }]).conversations.chat.messages.find((message) => message.state === 'streaming')
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
        const attach = (uri: string) => { const input = composer()?.onFiles; assert.ok(typeof input === 'function'); const files = [{ uri, name: uri.split('/').at(-1), size: 3 }]; input(files); return files }
        const sendDraft = () => { const send = composer()?.onSend; assert.ok(typeof send === 'function'); send() }
        const storedDraft = () => JSON.parse(stored.get(`vibeface.${networkKey('bot-drafts', 'network')}`) ?? '{}').chat
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
        assert.equal(mainSaves.length, 2, 'editing creates a separate draft that can save while the first is preparing')
        preparing.resolve(new Uint8Array([1, 2, 3]).buffer)
        await mainSaves[0]; await settleComponent(); mobile()
        assert.equal(composer()?.value, '后来输入的内容')
        assert.equal(composer()?.files, laterFiles, 'local acceptance cannot remove attachments selected after submission')
        assert.equal(storedDraft(), '后来输入的内容')
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
        sendDraft(); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, optionStart + 2, 'changing only target saves a distinct input with the same text and attachment')
        routine = undefined
        chat.render('default'); chat.render('default')
        sendDraft(); sendDraft(); await settleComponent()
        assert.equal(mainSaves.length, optionStart + 3, 'returning to Main preserves a separate input identity')
        routine = pendingRoutine
        chat.render('default'); chat.render('default')
        changingOptions.resolve(new Uint8Array([13, 14, 15]).buffer)
        await Promise.all(mainSaves.slice(optionStart)); await settleComponent(); mobile()
        assert.equal(composer()?.value, '切换目标仍是独立输入')
        assert.equal(composer()?.files, optionFiles, 'an earlier target cannot clear the current attachment')
        assert.equal(storedDraft(), '切换目标仍是独立输入')
        const optionMainTarget = { kind: 'bot_main', bot_id: botA.bot_id }, optionRoutineTarget = { kind: 'routine', bot_id: botA.bot_id, trigger: pendingRoutine }
        assert.deepEqual(messages.slice(optionMessages).map(message => JSON.stringify([message.target, message.intent])).sort(), [[optionMainTarget, 'continue'], [optionRoutineTarget, 'continue'], [optionMainTarget, 'continue']].map(value => JSON.stringify(value)).sort(), 'chat input steers by default')
        sendDraft(); await mainSaves.at(-1); await settleComponent(); mobile()
        assert.equal(mainSaves.length, optionStart + 4)
        assert.equal(composer()?.value, '')
        assert.equal(storedDraft(), '', 'the matching target still clears after durable acceptance')
        assert.deepEqual(messages.slice(optionMessages).map(message => JSON.stringify([message.target, message.intent])).sort(), [[optionMainTarget, 'continue'], [optionRoutineTarget, 'continue'], [optionMainTarget, 'continue'], [optionRoutineTarget, 'continue']].map(value => JSON.stringify(value)).sort())
        assert.equal(new Set(messages.slice(optionMessages).map(message => message.message_id)).size, 4)
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
        const controller = '../../../../../apps/mobile/node_modules/react-native-keyboard-controller/src/components/KeyboardChatScrollView/'
        const helpers = componentHarness(controller + 'useChatKeyboard/helpers.ts', {})
        const scrollOffset = { value: 0 }, contentOffsetY = { value: 0 }, keyboardPadding = { value: 0 }
        let reactPadding = () => {}
        const padding = componentHarness(controller + 'useExtraContentPadding/index.ts', {
          'react-native': { Platform: platform }, '../../../architecture': { IS_FABRIC: true },
          '../../../reanimated': { useAnimatedReaction(read: () => number, react: (current: number, previous: number | null) => void) {
            let previous: number | null = null
            reactPadding = () => { const current = read(); react(current, previous); previous = current }
            reactPadding()
          } },
          '../../utils/safeScrollTo': { safeScrollTo() { assert.fail('iOS Fabric must update the inset and offset together') } },
          '../useChatKeyboard/helpers': helpers.module,
        })
        padding.render('useExtraContentPadding', {
          scrollViewRef: {}, extraContentPadding: dockHeight, keyboardPadding, blankSpace: { value: 0 },
          scroll: scrollOffset, contentOffsetY, layout: { value: { height: 844 } }, size: { value: { height: 2000 } },
          inverted: true, keyboardLiftBehavior: scroll?.props.keyboardLiftBehavior, freeze: { value: false },
        })
        measure({ nativeEvent: { layout: { height: 94 } } })
        reactPadding()
        assert.equal(contentOffsetY.value, -94, 'the installed hook reserves the complete dock on its first measurement')
        scrollOffset.value = contentOffsetY.value
        measure({ nativeEvent: { layout: { height: 94 } } }); reactPadding()
        assert.equal(contentOffsetY.value, -94, 'the same measurement must not add a second inset correction')
        measure({ nativeEvent: { layout: { height: 190 } } }); reactPadding()
        assert.equal(contentOffsetY.value, -190, 'dock growth preserves the latest edge including the initial inset')
        scrollOffset.value = contentOffsetY.value
        measure({ nativeEvent: { layout: { height: 94 } } }); reactPadding()
        assert.equal(contentOffsetY.value, -94, 'dock shrink preserves the latest edge')
        assert.equal(emptyPadding().paddingBottom, 94)
        keyboard.height.value = -168; keyboard.progress.value = 0.5
        assert.equal(emptyPadding().paddingBottom, 245, 'safe area compensation follows keyboard progress')
        keyboard.height.value = -336; keyboard.progress.value = 1
        assert.equal(844 - emptyPadding().paddingBottom, 448, 'the open keyboard and dock reserve H + K - S')
        const offsets: number[] = [], ref = flat?.props.ref
        assert.ok(ref && typeof ref === 'object' && 'current' in ref)
        ref.current = { scrollToOffset: ({ offset }: { offset: number }) => offsets.push(offset) }
        scrollOffset.value = 400; keyboardPadding.value = 302
        measure({ nativeEvent: { layout: { height: 190 } } })
        reactPadding()
        assert.equal(contentOffsetY.value, -94, 'the installed hook issues no offset change while reading older messages')
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
          ['light', true, 'systemUltraThinMaterialLight', 'transparent'], ['dark', true, 'systemUltraThinMaterialDark', 'transparent'],
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
        const saved = [...outboxItems.values()].find((item) => item.method === 'message' && item.params.message_id === head.message_id)
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
        const history = { events: [{ event_hash: 'a'.repeat(64), created_at_ms: 1, scope_id: head.conversation_id, kind: 'input_posted', speaker: { kind: 'human', device_id: 'client' }, body: { ...messages[before], type: 'input_posted' } }], coverage: { limit: 128, truncated: false, scope: 'workspace' } }
        rpcConnection.rpc = async (method, body = {}) => { closeCalls.push(method); return method === 'workspace_history' ? history : originalRpc(method, body) }
        let closeWork = Promise.resolve()
        const status = componentHarness('./workspace-chat.tsx', {
          '@vibeface/client/workspace/context.tsx': { useWorkspace: () => render() }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
          '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, action: () => Promise<void>) => { closeWork = action(); return closeWork } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
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
        const releasing = [...outboxItems.values()].find((item) => item.method === 'message' && item.params.message_id === head.message_id)
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
        const checked = [...outboxItems.values()].find((item) => item.method === 'message' && item.params.message_id === head.message_id)
        const control = [...outboxItems.values()].find((item) => item.method === 'workspace_command' && item.params.command_id === request.command_id)
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
    await t.test('a restored new Release identity cannot consume an older unwritten proof for the same input and CAS', async () => {
      const previous = [...outboxItems.values()].find((item) => item.method === 'workspace_command' && item.command_rejection !== undefined)
      assert.ok(previous?.command_rejection)
      const command = workspaceModel.parseWorkspaceCommand(previous.params.command)
      assert.ok(command.kind === 'control_input' && command.action.kind === 'release')
      const original = [...outboxItems.values()].find((item) => item.method === 'message' && item.params.message_id === command.message_id)
      assert.ok(original)
      const oldProof = structuredClone(previous)
      const secondId = previous.params.command_id === 'a'.repeat(64) ? 'b'.repeat(64) : 'a'.repeat(64)
      const second: OutboxItem = { ...previous, key: previous.key.replace(String(previous.params.command_id), secondId), params: { ...previous.params, command_id: secondId }, rejected: undefined, command_rejection: undefined }
      const receipt: InputReceipt = { message_id: command.message_id, target: workspaceModel.parsePendingMessage(original).target, stage: 'refused', actual_turn_id: null, reason: '原输入已拒绝', retry_safe: false, control_event: command.expected_event }
      // Restore the durable state of another command whose forward acknowledgement was lost; requestKey stays real.
      await memoryOutbox.put(second)
      await memoryOutbox.put({ ...original, receipt, delivery: 'unknown', release_expected_event: command.expected_event, release_command_id: secondId })
      inputReceiptOverrides.set(command.message_id, receipt)
      const before = messages.length, releases = inputReleases.length
      try {
        await render().queryInput(receipt)
        const waiting = render().pendingMessages.find((item) => item.message_id === command.message_id)
        assert.equal(waiting?.delivery, 'unknown', 'an old proof cannot settle a different current Release ID')
        assert.equal(await render().sendMessage('following restored Release'), true)
        await settleComponent()
        const following = render().pendingMessages.find((item) => item.text === 'following restored Release')
        assert.ok(following)
        await render().retryPendingMessage(command.message_id); await settleComponent()
        const retained = [...outboxItems.values()].find((item) => item.key === original.key)
        assert.ok(retained); assert.equal(retained.delivery, 'unknown'); assert.equal(retained.release_expected_event, command.expected_event); assert.equal(retained.release_command_id, secondId)
        assert.deepEqual(retained.params, original.params)
        assert.deepEqual(outboxItems.get(previous.key), oldProof, 'the first command keeps its exact durable proof')
        assert.deepEqual(outboxItems.get(second.key)?.params, second.params, 'the second command keeps its own request identity')
        assert.equal(inputReleases.length, releases, 'restored unknown control requests are only queried')
        assert.equal(messages.length, before, 'the same lane remains blocked until this command is reconciled')
        inputReceiptOverrides.set(command.message_id, { ...receipt, control_event: 'f'.repeat(64) })
        await render().queryInput(receipt); await settleComponent()
        assert.deepEqual(messages.slice(before).map((message) => message.message_id), [following.message_id])
      } finally {
        inputReceiptOverrides.set(command.message_id, { ...receipt, control_event: 'f'.repeat(64) })
        await render().queryInput(receipt); await settleComponent()
        inputReceiptOverrides.clear()
      }
    })
  } finally {
    for (const effect of effects.values()) effect.cleanup?.()
  }
})

function componentHarness(path: string, dependencies: Record<string, unknown>, globals: Record<string, unknown> = {}, privateExport = '') {
  const hooks = hookHarness(true), { values, effects, afterRender } = hooks
  const focusEffects = new Map<number, { run: () => (() => void) | void; cleanup?: () => void }>()
  type Props = Record<string, unknown> & { name?: string; type?: string; role?: string; disabled?: boolean; value?: unknown; onChange?: (event: unknown) => void; onSubmit?: (event?: unknown) => void; onClick?: () => void; action?: () => Promise<unknown> }
  const elements: { type: unknown; props: Props; key?: string }[] = []
  let focused = true, output: unknown
  const react = {
    ...hooks.react,
    lazy: () => 'Lazy', Suspense: 'Suspense',
    useId: () => 'component',
    useSyncExternalStore: (_subscribe: unknown, snapshot: () => unknown) => snapshot(),
    useCallback(callback: unknown, dependencies: unknown[]) {
      const index = hooks.next()
      const previous = values[index] as { callback: unknown; dependencies: unknown[] } | undefined
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) values[index] = { callback, dependencies }
      return (values[index] as { callback: unknown }).callback
    },
    useMemo(read: () => unknown, dependencies: unknown[]) {
      const index = hooks.next(), previous = values[index] as { value: unknown; dependencies: unknown[] } | undefined
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) values[index] = { value: read(), dependencies }
      return (values[index] as { value: unknown }).value
    },
    useLayoutEffect: hooks.react.useEffect,
  }
  const jsx = (type: unknown, props: Props, key?: string) => { const element = key === undefined ? { type, props } : { type, props, key }; elements.push(element); return element }
  const ui = new Proxy({}, { get: (_target, name) => name === 'useAction' ? () => ({ busy: null, error: null, run() {}, clear() {} }) : String(name) })
  const module: { exports: Record<string, unknown> } = { exports: {} }
  const draftStorage = new Map<string, string>()
  const platformStorage = { getItem: (key: string) => draftStorage.get(key) ?? null, setItem: (key: string, value: string) => { draftStorage.set(key, value) } }
  const useFocusEffect = (run: () => (() => void) | void) => {
    const index = hooks.next(), previous = focusEffects.get(index)
    if (!previous || previous.run !== run) afterRender.push(() => {
      previous?.cleanup?.()
      focusEffects.set(index, { run, cleanup: focused ? run() ?? undefined : undefined })
    })
  }
  const require = (specifier: string): unknown => {
    if (specifier === '@vibeface/client/workspace/model.ts') {
      const override = dependencies[specifier]
      return { ...workspaceModel, ...(typeof override === 'object' && override !== null ? override : {}) }
    }
    if (specifier === '@vibeface/client/workspace/history.ts') return workspaceHistory
    if (specifier === '@/features/chat/memory-summary') {
      const filename = fileURLToPath(new URL('../../../../../apps/mobile/src/features/chat/memory-summary.tsx', import.meta.url))
      const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
      const memoryModule = { exports: {} }
      new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext({ Error, TextEncoder, ...globals })(memoryModule.exports, require, memoryModule)
      return memoryModule.exports
    }
    if (specifier === '@/features/chat/chrome' || specifier === './chrome') return { ChatMaterial: 'ChatMaterial', useChatChrome: () => ({ kind: 'solid' }), ...dependencies[specifier] as object }
    if (specifier === '@vibeface/client/workspace/storage.ts') return { storageOf, networkKey, ...dependencies[specifier] as object }
    if (specifier === '@/lib/platform') return { webPlatform: { storage: platformStorage }, ...dependencies[specifier] as object }
    if (specifier === '@/platform/files') return dependencies[specifier] ?? { privateFiles: { run: async (operation: (current: () => boolean) => Promise<unknown>) => operation(() => true), retain: (uri: string) => ({ uri }), release() {}, releasePicker() {} } }
    if (specifier === '@/platform') return { mobilePlatform: { storage: platformStorage }, ...dependencies[specifier] as object }
    if (specifier === '@tanstack/react-router') return { Link: 'Link', useNavigate: () => () => {}, useRouterState: ({ select }: { select: (state: unknown) => unknown }) => select({ location: { search: {} } }), ...dependencies[specifier] as object }
    return specifier === 'react' ? react : specifier === 'react/jsx-runtime' ? { jsx, jsxs: jsx } : specifier === 'expo-router' ? { Link: 'Link', useFocusEffect, ...dependencies[specifier] as object } : dependencies[specifier] ?? ui
  }
  const filename = fileURLToPath(new URL(path, import.meta.url))
  const source = readFileSync(filename, 'utf8') + (privateExport ? `\nexport { ${privateExport} }\n` : '')
  const compiled = transpileModule(source, { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
  new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext({ Map, Error, TextEncoder, ...globals })(module.exports, require, module)
  return {
    render(name: string, props: unknown = {}) {
      hooks.reset(); elements.length = 0
      const component = module.exports[name]
      assert.ok(typeof component === 'function')
      output = component(props)
      afterRender.splice(0).forEach(run => run())
      return elements
    },
    focus() { if (!focused) { focused = true; for (const effect of focusEffects.values()) effect.cleanup = effect.run() ?? undefined } },
    blur() { if (focused) { focused = false; for (const effect of focusEffects.values()) { effect.cleanup?.(); effect.cleanup = undefined } } },
    close() { for (const effect of [...effects.values(), ...focusEffects.values()]) effect.cleanup?.() },
    get output() { return output },
    module: module.exports,
  }
}

test("anonymous workspace navigation preserves the requested page without redirecting twice", async (t) => {
  for (const [requested, resolved] of [
    ['/', undefined],
    ['/settings', undefined],
    ['/b/bot?conversation=chat#latest', undefined],
    ['/settings', '/join'],
    ['/b/bot?conversation=chat#latest', '/connect'],
    ['/settings', '/settings'],
  ] satisfies [string, string | undefined][]) await t.test(`${requested} after ${resolved ?? 'initial load'}`, () => {
    const location = (href: string) => ({ href, pathname: new URL(href, 'https://app.vibeface.dev').pathname })
    const state = { location: location(requested), resolvedLocation: resolved ? location(resolved) : undefined }
    const app = componentHarness('../../routes/_app.tsx', {
      '@tanstack/react-router': {
        createFileRoute: () => () => ({}), Navigate: 'Navigate',
        useNavigate: () => () => {},
        useRouterState: ({ select }: { select: (value: typeof state) => unknown }) => select(state),
      },
      '@vibeface/client/identity/access.ts': { accessOf },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({
        ready: null, phase: 'disconnected', entries: [], initialInvitation: '', joining: false, link: { kind: 'idle' },
      }) },
      '@/features/identity/onboarding-flag': { useOnboarding: () => null },
    }, {}, 'AppLayout')
    try {
      const navigation = app.render('AppLayout').filter(({ type }) => type === 'Navigate')
      assert.equal(navigation.length, 1)
      assert.equal(navigation[0]?.props.to, '/connect')
      assert.equal(navigation[0]?.props.replace, true)
      const search: Record<string, string> = requested === '/' ? {} : { redirect: requested }
      assert.equal(JSON.stringify(navigation[0]?.props.search), JSON.stringify(search))
      state.location = location('/connect?' + new URLSearchParams(search))
      for (let render = 0; render < 3; render++) {
        assert.equal(app.render('AppLayout').filter(({ type }) => type === 'Navigate').length, 0,
          'the pending connect page must not become another return target')
      }
    } finally { app.close() }
  })
})

const pendingResult = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settleComponent = () => new Promise(setImmediate)

const elementText = (node: unknown): string => typeof node === 'string' ? node : Array.isArray(node) ? node.map(elementText).join('')
  : node && typeof node === 'object' && 'props' in node && node.props && typeof node.props === 'object' && 'children' in node.props ? elementText(node.props.children) : ''

test('native chat glass follows accessibility events without replacing its input content', async () => {
  const initial = pendingResult(), platform = { OS: 'ios' }
  const absoluteFill = { position: 'absolute', top: 0, right: 0, bottom: 0, left: 0 }
  let theme = 'light', supported = true, liquid = true, removed = 0, reads = 0, change = (_enabled: boolean) => {}
  const app = componentHarness('../../../../../apps/mobile/src/features/chat/chrome.tsx', {
    'expo-glass-effect': { GlassView: 'GlassView', isGlassEffectAPIAvailable: () => supported, isLiquidGlassAvailable: () => liquid },
    'react-native': { View: 'View', Platform: platform, StyleSheet: { absoluteFill }, AccessibilityInfo: {
      isReduceTransparencyEnabled: () => { reads++; return initial.promise },
      addEventListener: (name: string, listener: typeof change) => { assert.equal(name, 'reduceTransparencyChanged'); change = listener; return { remove() { removed++ } } },
    } },
    'uniwind': { useUniwind: () => ({ theme }) },
  })
  const read = () => {
    app.render('useChatChrome')
    const chrome = app.output
    assert.ok(chrome && typeof chrome === 'object' && 'kind' in chrome)
    return chrome
  }
  const input = { type: 'TextInput', props: { value: '保留输入中的草稿', selection: { start: 2, end: 2 } } }
  const material = (chrome: ReturnType<typeof read>) => {
    const elements = app.render('ChatMaterial', { chrome, children: input })
    const root = elements.at(-1)
    assert.equal(root?.type, 'View')
    assert.ok(Array.isArray(root?.props.children))
    assert.equal(root.props.children[1], input, 'the same input stays in the same child slot beneath a fixed View')
    const background = elements.find(({ props }) => props.pointerEvents === 'none')
    assert.ok(background)
    const style = background.props.style
    assert.deepEqual(Object.assign({}, ...(Array.isArray(style) ? style : [style])), { ...absoluteFill, borderRadius: 26 }, 'both material backgrounds fill their native parent')
    return elements
  }
  try {
    assert.equal(read().kind, 'solid', 'the first render waits for the accessibility setting')
    change(false)
    initial.resolve(true)
    await settleComponent()
    const glass = read()
    assert.equal(glass.kind, 'glass', 'a late initial read cannot overwrite a newer settings event')
    assert.equal(material(glass).find(({ type }) => type === 'GlassView')?.props.colorScheme, 'light')
    theme = 'dark'
    assert.equal(material(read()).find(({ type }) => type === 'GlassView')?.props.colorScheme, 'dark')
    change(true)
    const solid = read()
    assert.equal(solid.kind, 'solid')
    assert.equal(material(solid).some(({ type }) => type === 'GlassView'), false)
    change(false); liquid = false
    assert.equal(read().kind, 'solid', 'Liquid Glass availability is checked independently of the API')
    assert.equal(removed, 1)
    supported = false; liquid = true
    assert.equal(read().kind, 'solid')
    supported = true; platform.OS = 'android'
    assert.equal(read().kind, 'solid')
    assert.equal(reads, 1, 'unsupported platforms do not subscribe or read iOS accessibility settings')
  } finally { app.close() }
})

test('opening AI settings connects only Cursor and retains focus until it can recheck CLI login', async (t) => {
  for (const outcome of ['returned', 'read-back', 'unreadable', 'connection-changed', 'recheck-failed', 'same-device-reconnected']) await t.test(outcome, async () => {
    const unreadable = outcome === 'unreadable' || outcome === 'same-device-reconnected'
    const provider: ProviderStatus = {
      provider: 'cursor', authentication_source: 'local_cli_login', roles: [], supports: ['agent', 'execution'], bin: '/fixture/cursor-sdk-bridge', app_server: null,
      endpoint: null, execution_model: '', execution_readiness: { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } }, setup: null, installed: false, home: null, sessions: null, env: [],
      state: 'off', issues: [], readiness: { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } },
      probe: { provider: 'cursor', config_id: 'fixture', checked_at_ms: null, launch: { state: 'unchecked' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
      observed: { installed: { status: 'rejected', evidence: 'binary_discovery' }, launchable: { status: 'unknown', evidence: 'launch_prerequisites' }, authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
    }
    const otherProvider: ProviderStatus = { ...provider, provider: 'codex', authentication_source: 'private_login', roles: ['execution'], probe: { ...provider.probe, provider: 'codex' } }
    const initial: NodeSettingsResponse = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: null }, providers: [provider, otherProvider], display_paths: {}, suggested_roots: [] }
    const signedOut: NodeSettingsResponse = { ...initial, providers: [{ ...provider, bin: '/fixture/agent', installed: true,
      setup: { directory: null, command: "'/fixture/agent' login" }, probe: { ...provider.probe, checked_at_ms: 1, authentication: 'sign_in_required' } }, otherProvider] }
    const signedIn: NodeSettingsResponse = { ...signedOut, providers: signedOut.providers.map((item) => item.provider === 'cursor' ? { ...item, probe: { ...item.probe, checked_at_ms: 2, authentication: 'authenticated' } } : item) }
    const first = pendingResult(), second = pendingResult(), readback = pendingResult(), replacementRead = pendingResult(), calls: { method: string; params: unknown }[] = []
    let current = true, reads = 0
    const connection = { entry: { device_id: 'fixture-node' }, isCurrent: () => current, request: async (method: string, params?: unknown) => {
      calls.push({ method, params })
      if (method === 'node_settings') return ++reads === 2 && unreadable ? readback.promise : signedOut
      return calls.filter((call) => call.method === 'configure_provider').length === 1 ? first.promise : second.promise
    } }
    const replacementCalls: { method: string; params: unknown }[] = []
    const replacement = { entry: connection.entry, isCurrent: () => !current, request: async (method: string, params?: unknown) => {
      replacementCalls.push({ method, params })
      if (method === 'node_settings' && outcome === 'same-device-reconnected') return replacementRead.promise
      return signedIn
    } }
    let ready = connection
    let focused = () => {}
    const app = componentHarness('../devices/ai-settings.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ isCurrent: () => true, refresh: async () => {} }) },
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'Tasks' } } },
    }, { window: { addEventListener(_event: string, callback: () => void) { focused = callback }, removeEventListener() { focused = () => {} } } })
    const render = () => app.render('AiSettings', { compact: true })
    try {
      render(); await settleComponent(); render(); render()
      const writes = () => calls.filter((call) => call.method === 'configure_provider')
      assert.equal(writes().length, 1, 'entering settings discovers Cursor once without clicking automatic connection')
      const cached = render().find(({ props }) => props.item !== undefined)
      assert.equal(cached?.props.busy, 'connect_local')
      assert.equal(nodeSettings.parseProvider(cached?.props.item).probe.authentication, 'sign_in_required', 'the CLI login command is already available during the old probe')
      focused(); focused()
      assert.equal(writes().length, 1, 'focus cannot duplicate an in-flight connection write')
      if (outcome === 'connection-changed') {
        current = false; ready = replacement
        render(); await settleComponent(); render(); render()
        first.resolve(signedOut)
        for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
        assert.equal(writes().length, 1, 'an old connection never consumes its pending focus')
        assert.deepEqual(JSON.parse(JSON.stringify(replacementCalls)), [
          { method: 'node_settings' }, { method: 'configure_provider', params: { provider: 'cursor', connect_local: true } },
        ], 'the new connection gets only its own automatic check')
        assert.equal(nodeSettings.parseProvider(render().find(({ props }) => props.item !== undefined)?.props.item).probe.authentication, 'authenticated')
        return
      }
      if (outcome === 'read-back' || unreadable) first.reject(new Error('client response timed out; outcome may be unknown'))
      else first.resolve(signedOut)
      await settleComponent(); render()
      if (unreadable) {
        assert.equal(calls.at(-1)?.method, 'node_settings', 'an unknown connection write is read back before any retry')
        readback.reject(new Error('device read failed'))
        await settleComponent(); render()
        focused(); focused()
        for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
        assert.equal(writes().length, 1, 'focus cannot resend an unknown write before it is reconciled')
        const elements = render()
        assert.equal(elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')?.props.disabled, true)
        if (outcome === 'same-device-reconnected') {
          const oldCalls = JSON.parse(JSON.stringify(calls))
          current = false; ready = replacement
          assert.equal(ready.entry.device_id, connection.entry.device_id, 'the component stays mounted for the same device')
          render(); await settleComponent(); render()
          assert.deepEqual(JSON.parse(JSON.stringify(replacementCalls)), [{ method: 'node_settings' }], 'the new connection waits for its own settings read')
          replacementRead.resolve(signedOut)
          for (let turn = 0; turn < 4; turn++) { await settleComponent(); render() }
          assert.deepEqual(JSON.parse(JSON.stringify(replacementCalls)), [
            { method: 'node_settings' }, { method: 'configure_provider', params: { provider: 'cursor', connect_local: true } },
          ], 'a successful read on the new connection automatically connects Cursor exactly once')
          assert.deepEqual(JSON.parse(JSON.stringify(calls)), oldCalls, 'reconnection never replays the unknown write on the old connection')
          const statuses = render().filter(({ props }) => props.item !== undefined).map(({ props }) => nodeSettings.parseProvider(props.item))
          assert.equal(statuses.find((item) => item.provider === 'cursor')?.probe.authentication, 'authenticated')
          assert.deepEqual(statuses.map((item) => ({ provider: item.provider, roles: item.roles })), [
            { provider: 'cursor', roles: [] }, { provider: 'codex', roles: ['execution'] },
          ], 'reconnection preserves provider roles')
          return
        }
        const retry: unknown = elements.find(({ type }) => type === 'InlineError')?.props.action
        assert.ok(retry && typeof retry === 'object' && 'label' in retry && retry.label === '重新读取' && 'onClick' in retry && typeof retry.onClick === 'function')
        retry.onClick()
      }
      for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
      assert.equal(writes().length, 2, 'the merged busy focus is consumed once after the connection is safe')
      assert.deepEqual(calls.map((call) => call.method), outcome === 'returned' || outcome === 'recheck-failed'
        ? ['node_settings', 'configure_provider', 'configure_provider']
        : outcome === 'read-back' ? ['node_settings', 'configure_provider', 'node_settings', 'configure_provider']
          : ['node_settings', 'configure_provider', 'node_settings', 'node_settings', 'configure_provider'], 'unknown writes are read before the queued check starts')
      if (outcome === 'recheck-failed') second.reject(new Error('client response timed out; outcome may be unknown'))
      else second.resolve(signedIn)
      for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
      const card = render().find(({ props }) => props.item !== undefined)
      const status = nodeSettings.parseProvider(card?.props.item)
      assert.equal(status.probe.authentication, outcome === 'recheck-failed' ? 'sign_in_required' : 'authenticated')
      assert.deepEqual(status.roles, [])
      assert.equal(writes().length, 2, 'no further focus means no additional automatic attempt')
      if (outcome === 'recheck-failed') {
        assert.deepEqual(calls.map((call) => call.method), ['node_settings', 'configure_provider', 'configure_provider', 'node_settings'])
        assert.ok(render().some(({ type }) => type === 'InlineError'), 'a failed queued check remains visible instead of retrying itself')
      }
      assert.ok(writes().every((call) => JSON.stringify(call.params) === JSON.stringify({ provider: 'cursor', connect_local: true })), 'automatic connection never changes roles or other providers')
    } finally { app.close() }
  })
})

test("automatic connection checks the three local services without changing uses and reads back unknown writes", async (t) => {
  for (const outcome of ['ready', 'first-failed', 'unreadable', 'connection-changed']) await t.test(outcome, async () => {
    const provider: ProviderStatus = {
      provider: 'codex', authentication_source: 'private_login', roles: ['execution'], supports: ['agent', 'execution'], bin: '/fixture/codex', app_server: null,
      endpoint: null, execution_model: '', execution_readiness: { state: 'starting' }, setup: null, installed: true, home: null, sessions: null, env: [],
      state: 'starting', issues: [], readiness: { state: 'starting' },
      probe: { provider: 'codex', config_id: 'fixture', checked_at_ms: null, launch: { state: 'starting' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
      observed: { installed: { status: 'confirmed', evidence: 'binary_discovery' }, launchable: { status: 'unknown', evidence: 'app_server_starting' }, authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
    }
    const initial: NodeSettingsResponse = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: null }, providers: [provider], display_paths: {}, suggested_roots: [] }
    const first = pendingResult(), readback = pendingResult()
    let current = true, reads = 0
    const calls: { method: string; params: unknown }[] = []
    const connection = { isCurrent: () => current, request: async (method: string, params?: unknown) => {
      calls.push({ method, params })
      if (method === 'node_settings') return ++reads === 1 ? initial : readback.promise
      if (calls.filter((call) => call.method === 'configure_provider').length === 1) return first.promise
      if (outcome === 'unreadable') throw new Error('client response timed out; outcome may be unknown')
      return initial
    } }
    const workspace = { isCurrent: () => current, refresh: async () => {} }
    const timers = new Set<unknown>()
    let focused = () => {}
    const app = componentHarness('../devices/ai-settings.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: connection }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'Tasks' } } },
    }, {
      window: { addEventListener(_event: string, callback: () => void) { focused = callback }, removeEventListener() { focused = () => {} } },
      setTimeout(callback: () => void) { timers.add(callback); return callback }, clearTimeout(callback: unknown) { timers.delete(callback) },
    })
    const render = () => app.render('AiSettings', { compact: true, targetName: '远端工作机' })
    try {
      render(); await settleComponent()
      let elements = render()
      assert.ok(elements.some((element) => elementText(element).includes('在「远端工作机」上检测 Codex、Claude Code 和 Cursor')))
      const connect = elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')
      assert.ok(connect?.props.onClick)
      connect.props.onClick(); connect.props.onClick()
      assert.equal(calls.filter((call) => call.method === 'configure_provider').length, 1, 'duplicate clicks do not submit again')
      elements = render()
      assert.equal(timers.size, 0, 'starting-state polling pauses while the connection write is pending')
      focused()
      assert.equal(calls.length, 2, 'a focus read cannot replace the active write sequence')
      assert.equal(elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')?.props['aria-busy'], true)
      if (outcome === 'connection-changed') {
        current = false
        first.resolve({ ...initial, settings: { roots: ['/stale'], manager: null } })
        await settleComponent()
        assert.equal(calls.length, 2, 'a stale write cannot continue on a different connection')
        assert.ok(!render().some(({ props }) => Array.isArray(props.roots) && props.roots.includes('/stale')))
        return
      }
      if (outcome === 'ready') first.resolve(initial)
      else {
        first.reject(new Error('client response timed out; outcome may be unknown'))
        await settleComponent()
        assert.equal(calls.at(-1)?.method, 'node_settings')
        assert.equal(calls.filter((call) => call.method === 'configure_provider').length, 1, 'the unknown write is read back before continuing')
        if (outcome === 'unreadable') readback.reject(new Error('device read failed'))
        else readback.resolve(initial)
      }
      await settleComponent(); await settleComponent()
      const writes = calls.filter((call) => call.method === 'configure_provider').map((call) => JSON.parse(JSON.stringify(call.params)))
      assert.deepEqual(writes, [{ provider: 'codex', connect_local: true }, { provider: 'claude', connect_local: true }, { provider: 'cursor', connect_local: true }])
      elements = render()
      const button = elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')
      assert.equal(button?.props.disabled, outcome === 'unreadable', 'a failed read blocks another attempt until the settings can be read')
      const result = elements.find(({ props }) => props.role === 'status')
      assert.match(elementText(result), outcome === 'ready' ? /检测已完成/ : /部分服务未完成/)
      if (outcome !== 'ready') assert.ok(elements.some(({ type }) => type === 'InlineError'), 'the first failure remains visible after the other service completes')
    } finally { app.close() }
  })
})

test("compact system provider cards use external service evidence without local prerequisites", async (t) => {
  for (const mode of ['system', 'spawn'] satisfies NonNullable<ProviderStatus['app_server']>['mode'][]) for (const installed of [false, true]) for (const authentication of ['authenticated', 'unverified', 'sign_in_required'] satisfies ProviderStatus['probe']['authentication'][]) await t.test(`${mode} installed=${installed} ${authentication}`, () => {
    const item: ProviderStatus = {
      provider: 'codex', authentication_source: 'private_login', roles: ['execution'], supports: ['agent', 'execution'], bin: '/fixture/codex',
      app_server: { mode, socket: '/fixture/codex.sock', link: { state: 'connected', socket: '/fixture/codex.sock', user_agent: 'fixture' } },
      endpoint: null, execution_model: '', execution_readiness: { state: 'ready' }, setup: null, installed, home: null, sessions: null,
      env: [{ name: 'FIXTURE_KEY', set: false, inherited: false, required: true }], state: 'ready', issues: [], readiness: { state: 'ready' },
      probe: { provider: 'codex', config_id: 'fixture', checked_at_ms: null, launch: { state: 'launchable' }, authentication, catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
      observed: { installed: { status: 'rejected', evidence: 'binary_discovery' }, launchable: { status: 'confirmed', evidence: 'app_server_handshake' }, authenticated: { status: 'confirmed', evidence: 'provider_probe' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
    }
    const response: NodeSettingsResponse = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: null }, providers: [item], display_paths: {}, suggested_roots: [] }
    const app = componentHarness('../devices/ai-settings.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'Tasks' }, secret: { icon: 'Key' } } },
    }, {}, 'ProviderCard')
    try {
      const elements = app.render('ProviderCard', { item, response, onboarding: false, compact: true, busy: '', models: null, error: '' })
      const summary = elements.find(({ type, props }) => type === 'span' && props.className === 'truncate')
      assert.equal(elementText(summary), mode === 'system' || installed ? '可用' : '未安装', 'the main state follows readiness for system services')
      const evidence = elements.filter(({ type }) => type === 'dl').map(elementText).join('')
      if (mode === 'system') {
        assert.match(evidence, /程序外部服务/)
        assert.ok(evidence.includes(`认证${{ authenticated: '已认证', unverified: '未验证', sign_in_required: '需登录' }[authentication]}`))
        assert.equal(elements.filter(({ props }) => props.name === 'FIXTURE_KEY').length, 0, 'an external service never requests the private key')
        assert.ok(!elements.some((element) => elementText(element).includes('找不到 Codex 程序')))
      } else {
        assert.ok(evidence.includes(`程序${installed ? '已找到' : '未找到'}`))
        assert.match(evidence, /认证需填写密钥/)
        assert.equal(elements.filter(({ props }) => props.name === 'FIXTURE_KEY').length, installed ? 1 : 0)
        assert.equal(elements.some((element) => elementText(element).includes('找不到 Codex 程序')), !installed)
      }
    } finally { app.close() }
  })
})

test("provider cards keep summary readiness, setup and execution details in their own roles", async (t) => {
  const ready: ProviderReadiness = { state: 'ready' }
  const signIn: ProviderReadiness = { state: 'needs_setup', issue: { stage: 'authentication', code: 'sign_in_required' } }
  const model: ProviderReadiness = { state: 'unverified', issue: { stage: 'model_list', code: 'model_unverified' } }
  const signInText = 'Codex 需要完成认证，请在运行设备登录或配置 API 密钥'
  const modelText = 'Codex 尚未确认这个模型，可发送消息或先检查可用性'
  const cases: (Pick<ProviderStatus, 'roles' | 'readiness' | 'execution_readiness'> & {
    name: string; label: string; reason: string; setup: boolean; executionLabel: string | null; executionReason: string
  })[] = [
    { name: 'execution-only unverified', roles: ['execution'], readiness: ready, execution_readiness: model, label: '未验证，可尝试', reason: modelText, setup: true, executionLabel: '未验证，可尝试', executionReason: modelText },
    { name: 'execution-only ready', roles: ['execution'], readiness: signIn, execution_readiness: ready, label: '可用', reason: '', setup: false, executionLabel: '可用', executionReason: '' },
    { name: 'Bot ready and execution unverified', roles: ['agent', 'execution'], readiness: ready, execution_readiness: model, label: '可用', reason: '', setup: false, executionLabel: '未验证，可尝试', executionReason: modelText },
    { name: 'Bot needs setup and execution ready', roles: ['execution', 'agent'], readiness: signIn, execution_readiness: ready, label: '需设置', reason: signInText, setup: true, executionLabel: '可用', executionReason: '' },
    { name: 'no roles', roles: [], readiness: signIn, execution_readiness: ready, label: '需设置', reason: signInText, setup: false, executionLabel: null, executionReason: '' },
  ]
  const text = (node: unknown): string => typeof node === 'string' ? node : Array.isArray(node) ? node.map(text).join('')
    : node && typeof node === 'object' && 'props' in node && node.props && typeof node.props === 'object' && 'children' in node.props ? text(node.props.children) : ''
  const kit = { toneText: { neutral: 'neutral', success: 'success', warning: 'warning' }, providerIcons: { codex: 'CodexIcon' }, Details: 'Details', FolderPath: 'FolderPath', CopyableId: 'CopyableId', BusyIcon: 'BusyIcon' }
  for (const platform of ['web', 'mobile']) for (const example of cases) await t.test(`${platform} ${example.name}`, () => {
    const item: ProviderStatus = {
      provider: 'codex', authentication_source: 'private_login', roles: example.roles, supports: ['agent', 'execution'], bin: '/usr/bin/codex', app_server: null,
      endpoint: null, execution_model: 'saved-model', execution_readiness: example.execution_readiness,
      setup: { directory: '/private/provider', command: 'codex --home /private/provider login' }, installed: true,
      home: null, sessions: null, env: [], state: example.readiness.state, issues: [], readiness: example.readiness,
      observed: { installed: { status: 'confirmed', evidence: 'binary_discovery' }, launchable: { status: 'unknown', evidence: 'launch_prerequisites' }, authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
      probe: { provider: 'codex', config_id: 'config', checked_at_ms: null, launch: { state: 'launchable' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
    }
    const response: NodeSettingsResponse = {
      settings: { roots: [], manager: item.roles.includes('agent') ? { provider: 'codex', workspace: '/project', model: 'bot-model', reasoning_effort: null, backup: { automatic: false, candidates: [] } } : null },
      defaults: { workspace: '/project' }, readiness: { manager: item.readiness }, providers: [item], display_paths: {}, suggested_roots: [],
    }
    const name = platform === 'web' ? 'ProviderCard' : 'ProviderRow'
    const app = componentHarness(platform === 'web' ? '../devices/ai-settings.tsx' : '../../../../../apps/mobile/app/ai.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'ListChecks' } } },
      '@/components/app/kit': kit,
      '@/components/kit': kit,
    }, {}, name)
    const props = { item, response, onboarding: false, first: true, busy: '', models: null, error: '' }
    try {
      let elements = app.render(name, props)
      if (platform === 'mobile') {
        assert.equal(elements.some(({ props }) => props.label === '执行任务'), false, 'execution details start collapsed')
        const expand = elements.find(({ type, props }) => type === 'Pressable' && props.accessibilityRole === 'button')?.props.onPress
        assert.ok(typeof expand === 'function')
        expand()
        elements = app.render(name, props)
      }
      const children = elements.at(-1)?.props.children
      assert.ok(Array.isArray(children))
      assert.equal(text(children[0]), `Codex${example.label}`, 'the row header shows the selected summary state')
      const summary = text(children[2])
      for (const reason of [signInText, modelText]) assert.equal(summary.includes(reason), example.reason === reason, 'the summary shows only its own reason')
      assert.equal(elements.filter(({ props }) => props.value === item.setup?.command).length, example.setup ? 1 : 0, 'setup follows the selected readiness and enabled roles')
      const execution = elements.find(({ props }) => props.label === '执行任务')
      if (example.executionLabel === null) assert.equal(execution, undefined)
      else {
        assert.ok(execution)
        assert.equal(text(execution), `执行模型：${example.executionLabel}${example.executionReason}`, 'execution details show the saved execution readiness independently')
      }
    } finally { app.close() }
  })
})

test('webhook secret saves preserve failures and newer drafts on web and mobile', async (t) => {
  const settings = workspaceModel.parseTriageSettings({
    default_outlet: 'record', projects: [], webhook_url: '', webhook_secret_set: false,
    scan_codex_sessions: false, codex_sessions: null, decision_url: '', device_id: 'node', decision_tokens: [],
    outbox: { pending: 0, delivered: 0, dead: 0 },
    status: { budget: { day: '2026-11-11', used: 0, limit: 10, retry_at_ms: 0, paused_reason: null }, pending_events: 0, inflight_events: 0, unknown_events: 0 },
  })
  for (const platform of ['web', 'mobile']) for (const outcome of ['failed', 'saved', 'edited']) await t.test(`${platform} ${outcome}`, async () => {
    const held = pendingResult(), writes: unknown[] = []
    const save = (params: unknown) => { writes.push(params); return held.promise }
    const connection = { isCurrent: () => true, request: async (method: string, params?: unknown) => method === 'triage_settings' ? settings : save(params) }
    const name = platform === 'mobile' ? 'Triage' : 'TriageSettings'
    const app = componentHarness(platform === 'mobile' ? '../../../../../apps/mobile/app/external-chat.tsx' : '../devices/triage-settings.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: connection }) },
      '@vibeface/client/identity/copy.ts': { waitingCopy },
      '@vibeface/client/workspace/model.ts': workspaceModel,
      '@/lib/glossary': { terms: { externalChat: { label: '外部聊天', icon: 'Send' } } },
      '@/features/devices/remote-settings': { useCall: () => ({ value: settings, busy: '', error: null, call: (_key: string, _method: string, params: unknown) => save(params) }) },
    }, {}, platform === 'mobile' ? 'Triage' : '')
    const render = () => app.render(name)
    const field = () => {
      const input = render().find(({ props }) => platform === 'mobile' ? props.accessibilityLabel === '外部聊天密钥' : props.id === 'webhook-secret')
      assert.ok(input)
      return input.props
    }
    const edit = (value: string) => {
      const input = field()
      if (platform === 'mobile') { assert.ok(typeof input.onChangeText === 'function'); input.onChangeText(value) }
      else { assert.ok(typeof input.onChange === 'function'); input.onChange({ target: { value } }) }
    }
    try {
      render(); await settleComponent()
      if (platform === 'web') {
        const expand = render().find(({ type }) => type === 'SettingsRow')?.props.onClick
        assert.ok(typeof expand === 'function'); expand()
      }
      edit('first-secret-draft')
      if (platform === 'mobile') {
        const press = render().find(({ type, props }) => type === 'Button' && elementText(props.children) === '保存')?.props.onPress
        assert.ok(typeof press === 'function'); press()
      } else {
        const submit = render().find(({ type }) => type === 'form')?.props.onSubmit
        assert.ok(typeof submit === 'function'); submit({ preventDefault() {} })
      }
      assert.equal(writes.length, 1)
      assert.equal(JSON.parse(JSON.stringify(writes[0])).webhook_secret, 'first-secret-draft')
      if (outcome === 'edited') edit('newer-secret-draft')
      if (outcome === 'failed' && platform === 'web') held.reject(new Error('device offline'))
      else held.resolve(outcome === 'failed' ? null : settings)
      await settleComponent()
      assert.equal(field().value, outcome === 'saved' ? '' : outcome === 'edited' ? 'newer-secret-draft' : 'first-secret-draft')
    } finally { app.close() }
  })
})

test("mobile provider probes discard settings that arrive after another call or connection", async () => {
  for (const changed of ['call', 'connection', 'none']) {
    const held = pendingResult()
    let current = true, reads = 0, snapshot: unknown
    const connection = { isCurrent: () => current, request: async (method: string) => {
      if (method === 'node_settings') return ++reads === 1 ? 'initial settings' : held.promise
      return method === 'configure_node' ? 'new settings' : 'probe result'
    } }
    const app = componentHarness('../../../../../apps/mobile/src/features/devices/remote-settings.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: connection }) },
    })
    const useCall = app.module.useCall
    assert.ok(typeof useCall === 'function')
    app.module.Read = () => { snapshot = useCall((value: unknown) => value, 'node_settings'); return null }
    const state = () => {
      app.render('Read')
      assert.ok(snapshot && typeof snapshot === 'object' && 'call' in snapshot && typeof snapshot.call === 'function' && 'value' in snapshot)
      return { value: snapshot.value, call: snapshot.call }
    }
    try {
      state(); await settleComponent()
      assert.equal(state().value, 'initial settings')
      const probe = state().call('probe', 'provider_probe')
      await settleComponent()
      assert.equal(reads, 2)
      if (changed === 'call') await state().call('save', 'configure_node')
      if (changed === 'connection') current = false
      held.resolve('late settings')
      assert.equal(await probe, changed === 'none' ? 'probe result' : null)
      assert.equal(state().value, changed === 'none' ? 'late settings' : changed === 'call' ? 'new settings' : 'initial settings')
    } finally { app.close() }
  }
})

function nestedElements(elements: readonly { type: unknown; props: Record<string, unknown> }[], names: readonly string[]) {
  const expanded = [...elements]
  const pending = [...elements]
  const enqueue = (value: unknown) => {
    if (Array.isArray(value)) { value.forEach(enqueue); return }
    if (!value || typeof value !== "object" || !("type" in value) || !("props" in value)) return
    const element = value as { type: unknown; props: Record<string, unknown> }
    if (!expanded.includes(element)) expanded.push(element)
    pending.push(element)
    if (element.props && "children" in element.props) enqueue(element.props.children)
  }
  while (pending.length) {
    const element = pending.shift()!
    if (typeof element.type !== "function" || !names.includes(element.type.name)) continue
    enqueue(element.type(element.props))
  }
  return expanded
}

function componentText(node: unknown): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node)
  if (Array.isArray(node)) return node.map(componentText).join('')
  if (node && typeof node === 'object' && 'props' in node && node.props && typeof node.props === 'object' && 'children' in node.props) return componentText(node.props.children)
  return ''
}

type HarnessElement = { type: unknown; props: Record<string, unknown> }

function findActionProps(node: unknown, label: string): Record<string, unknown> | undefined {
  if (!node || typeof node !== 'object') return undefined
  if (Array.isArray(node)) {
    for (const item of node) {
      const found = findActionProps(item, label)
      if (found) return found
    }
    return undefined
  }
  if ('props' in node && node.props && typeof node.props === 'object') {
    const props = node.props as Record<string, unknown>
    if (componentText(node).includes(label) && (props.action || props.onClick)) return props
    if ('children' in props) return findActionProps(props.children, label)
  }
  return undefined
}

function conversationHarness(workspace: () => Record<string, unknown>, device: Record<string, unknown> = {}) {
  const chatGlobals = {
    setInterval: () => 1, clearInterval() {},
    matchMedia: () => ({ matches: false }),
    document: { getElementById: () => ({ focus() {}, scrollIntoView() {} }), hidden: false, addEventListener() {}, removeEventListener() {} },
    IntersectionObserver: class {
      observe() {}
      disconnect() {}
      constructor(callback: (entries: { isIntersecting: boolean }[]) => void) { callback([{ isIntersecting: false }]) }
    },
  }
  const app = componentHarness('../chat/conversation-screen.tsx', {
    '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) },
    '@/lib/glossary': { terms: { inbox: { label: '待处理', icon: 'InboxIcon' }, undelivered: { icon: 'UndeliveredIcon' }, approval: { icon: 'ApprovalIcon' } } },
    '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: workspace },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { can_manage: false, source: 'membership' } }, retryNow: () => {}, ...device }) },
    '@vibeface/client/identity/copy.ts': { setUpOnManager },
    '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters },
    '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage },
    '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger },
    '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks },
    '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval },
    '@vibeface/client/lib/errors.ts': errors,
  }, chatGlobals)
  const messages = componentHarness('../chat/elements/message.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: workspace },
    '@vibeface/client/chat/protocol.ts': { promptIntent, messageKey, replyStatusLabel },
    '@vibeface/client/lib/errors.ts': errors,
  })
  const render = (props: object = {}) => app.render('ConversationScreen', props)
  const composer = () => render().find(({ type }) => type === 'PromptInput')?.props as Record<string, unknown> | undefined
  const chatEntries = () => (render().find(({ type }) => type === 'Chat')?.props.entries ?? []) as { key: string; message?: { message_id: string; state?: string; text?: string }; node?: HarnessElement }[]
  const messageTree = (messageId: string) => {
    const message = chatEntries().find((entry) => entry.message?.message_id === messageId)?.message
    assert.ok(message)
    return messages.render('Message', { message })
  }
  return { app: { close: () => { app.close(); messages.close() } }, render, composer, chatEntries, messageTree, findAction: (label: string) => findActionProps(composer()?.above, label) }
}

const parityBot = parseBot({ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot', instructions: '', models: {}, skills: null, avatar: null, avatar_version: null, event_hash: 'bot-event', last_message: null, busy: null })
const parityNode = { device_id: 'worker', name: '工作设备', online: true, local: false, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready' as const, relay: false, roots: [], codex_connected: true } }
const parityWork = (extra: Partial<WorkRecord> = {}) => {
  const work = parseConversation({ ...context, works: [{
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    resolved: { provider: 'codex', model: null }, work_id: 'work', trigger: 'message:one', target_node: 'worker', cwd: '/project', title: '核对项目', prompt: '完整请求', state: 'running', summary: '✓ 读记录\n→ 核对结果\n· 等待同步', outcome: null, result_text: '', failure_reason: null, kind: 'codex', thread_id: 'session', ...extra }] }).works[0]
  assert.ok(work && work.resolved)
  return work
}
const paritySnapshot = (value: unknown) => value === null ? null : parseConversation(value)
const parityShared = () => ({
  ...historyActions, generation: 1, isCurrent: () => true, synced: true, connectionOnline: true, conversationId: 'chat', data: paritySnapshot(context),
  bots: [parityBot], selectedBot: parityBot, nodes: [parityNode], openWorks: [{ ...parityWork(), conversation_id: 'chat' }],
  overview: workspaceModel.parseWorkspaceOverview({}), questions: [] as ReturnType<typeof parseConversation>['questions'], answered: {} as Record<string, string>, liveProgress: [], writable: true,
  writeState: { state: 'writable' as const, conflicts: [] }, notifications: [], ignoredNotifications: new Set<string>(), pendingMessages: [], canSaveMessages: true, workspaceState: null,
  setView(_view: string) {}, selectBot: async (_bot: typeof parityBot) => {}, interruptWork: async (_work: string) => {}, sendMessage: async () => true,
  async readWork(input: { conversation_id: string; work_id: string }) {
    const work = this.data?.conversation?.conversation_id === input.conversation_id ? this.data.works.find((item) => item.work_id === input.work_id) : undefined
    if (!work) throw new Error('共享历史中找不到这个工作')
    return { work: { ...work, target: work.target ?? null, source_events: ['a'.repeat(64)], conversation_id: input.conversation_id }, coverage: { source: 'shared_history' as const, complete: false as const } }
  },
})

test('both pending input controls retry only a matching safe refusal and keep all scope targets', async () => {
  const targets: InputTarget[] = [{ kind: 'bot_main', bot_id: 'bot' }, { kind: 'task', task_id: 'task' }, { kind: 'channel_task', channel_id: 'channel', task_id: 'task', bot_id: 'bot' }, { kind: 'work', work_id: 'work' }, { kind: 'routine', bot_id: 'bot', trigger: 'schedule:one' }, { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }]
  for (const platform of ['web', 'native']) for (const target of targets) {
    let pending: PendingMessage = { report_results: true, message_id: 'original', conversation_id: 'chat', target, intent: 'continue', text: '保留原交代', sequence: 1, created_at_ms: 1, delivery: 'unknown', attachments: [{ hash: 'a'.repeat(64), size: 3, name: 'notes.txt' }] }
    const retries: string[] = [], closed: string[] = []
    const shared = { ...parityShared(), workspaceState: { channels: [], tasks: [], assignments: [], inputs: [] as WorkspaceDomainState['inputs'], coverage: { limit: 128, truncated: false } },
      queryInput: async () => { pending = { ...pending, delivery: 'accepted', receipt: { message_id: pending.message_id, target, stage: 'refused', retry_safe: true, control_event: 'c'.repeat(64), actual_turn_id: null, reason: '可以重试原输入' } } },
      retryPendingMessage: async (id: string) => { retries.push(id) },
      releaseMessagePending: async (id: string) => { closed.push(id) },
      sendMessage: async () => { throw new Error('must not create another input') },
    }
    const app = componentHarness(platform === 'web' ? './workspace-chat.tsx' : '../../../../../apps/mobile/src/features/workspace/workspace.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, action: () => Promise<void>) => action() }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_key: string, action: () => Promise<void>) => action() }) }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    })
    const render = () => app.render(platform === 'web' ? 'WorkspaceInputStatus' : 'InputStatus', { pending })
    const button = (label: string) => render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props
    const press = async (label: string) => { const control = button(label); assert.ok(control); const action = platform === 'web' ? control.onClick : control.onPress; assert.ok(typeof action === 'function'); action(); await settleComponent() }
    try {
      assert.equal(button('重试原输入'), undefined, 'unknown delivery alone never authorizes replay')
      assert.equal(button('关闭提醒'), undefined); assert.equal(button('不再发送'), undefined)
      await press('查询结果')
      const accepted = pending
      assert.ok(accepted.receipt)
      const acceptedReceipt = accepted.receipt
      assert.ok(button('重试原输入'))
      assert.ok(button('关闭提醒'))
      for (const delivery of ['saved', 'unknown', 'uploading', 'submitting'] as const) { pending = { ...accepted, delivery }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined) }
      pending = accepted
      await press('重试原输入')
      assert.deepEqual(retries, ['original'])
      assert.equal(pending, accepted, 'the UI never replaces the original input or its attachments')
      await press('关闭提醒')
      pending = { ...accepted, receipt: { ...acceptedReceipt, retry_safe: false, control_event: null } }
      assert.ok(button('关闭提醒'), 'closing a terminal reminder needs no retry authorization or control event')
      await press('关闭提醒')
      assert.deepEqual(closed, ['original', 'original'])
      for (const stage of ['unknown', 'entry_accepted', 'owner_accepted', 'provider_accepted', 'associated', 'submitting'] as const) { pending = { ...accepted, receipt: { ...acceptedReceipt, stage } }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined) }
      for (const receipt of [{ ...acceptedReceipt, retry_safe: false }, { ...acceptedReceipt, control_event: null }, { ...acceptedReceipt, control_event: '' }, { ...acceptedReceipt, control_event: ' ' }, { ...acceptedReceipt, message_id: 'different' }, { ...acceptedReceipt, target: { kind: 'bot_main' as const, bot_id: 'different' } }]) { pending = { ...accepted, receipt }; assert.equal(button('重试原输入'), undefined) }
      for (const receipt of [{ ...acceptedReceipt, message_id: 'different' }, { ...acceptedReceipt, target: { kind: 'bot_main' as const, bot_id: 'different' } }]) { pending = { ...accepted, receipt }; assert.equal(button('关闭提醒'), undefined) }
      for (const marker of [{ release_expected_event: 'c'.repeat(64) }, { release_command_id: 'a'.repeat(64) }]) {
        pending = { ...accepted, ...marker }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined)
        pending = { ...pending, delivery: 'saved', rejected: '尚未确认' }; assert.equal(button('不再发送'), undefined)
      }
      pending = { ...accepted, rejection_code: 'input_conflict', rejected: '原内容不同' }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined); assert.equal(button('不再发送'), undefined)
      pending = { ...accepted, delivery: 'unknown' }; assert.match(componentText(render()), /重试请求待确认/); assert.equal(button('重试原输入'), undefined)
      pending = { ...pending, rejected: '尚未确认' }; assert.equal(button('关闭提醒'), undefined); assert.equal(button('不再发送'), undefined)
      pending = accepted
      shared.writable = false; assert.equal(button('重试原输入'), undefined); assert.ok(button('关闭提醒'), 'closing only the local reminder needs no write permission'); shared.writable = true
      const queued: WorkspaceDomainState['inputs'][number] = { input: { report_results: true, message_id: pending.message_id, target, intent: 'defer', sequence: 1, text: pending.text }, conversation_id: 'chat', source_event: 'source', control_event: 'control', effective_order: 1, deferred: true, cancelled: false }
      shared.workspaceState.inputs = [queued]; assert.equal(button('重试原输入'), undefined)
      assert.ok(button('关闭提醒'), 'the matched refused receipt determines closure, not a queue flag')
      shared.workspaceState.inputs = [{ ...queued, deferred: false, cancelled: true }]; assert.equal(button('重试原输入'), undefined); assert.ok(button('关闭提醒'))
      pending = { ...accepted, delivery: 'saved', receipt: undefined, rejected: '原输入未保存', rejection_code: 'input_not_saved' }
      assert.equal(button('关闭提醒'), undefined); assert.ok(button('不再发送'))
      await press('不再发送'); assert.deepEqual(closed, ['original', 'original', 'original'])
    } finally { app.close() }
  }
})

test('new task creation uses the displayed default Bot after the catalog loads and the Bot filter changes', async () => {
  for (const mode of ['cold', 'filter']) {
    const other = { ...parityBot, bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' }
    const commands: unknown[] = []
    const shared = { ...parityShared(), bots: [] as ReturnType<typeof parseBot>[], selectedBot: undefined,
      workspaceCommand: async (command: unknown) => { commands.push(command); return { tasks: [{ task_id: 'created-task', revision: 1 }] } },
      sendMessage: async () => true,
    }
    let botId: string | undefined
    if (mode === 'filter') { shared.bots = [parityBot, other]; botId = parityBot.bot_id }
    const app = componentHarness('./workspace-tasks.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/workspace/storage.ts': { newId: () => 'created-task' },
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, action: () => Promise<void>) => action() }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
    }, {}, 'CreateWorkspaceTask')
    const render = () => app.render('CreateWorkspaceTask', { open: true, onOpenChange() {}, botId })
    const owner = () => render().find(({ type }) => type === 'NativeSelect')?.props.value
    try {
      if (mode === 'cold') { assert.equal(owner(), ''); shared.bots = [parityBot] }
      else { assert.equal(owner(), parityBot.bot_id); botId = other.bot_id; render() }
      const expected = mode === 'cold' ? parityBot.bot_id : other.bot_id
      assert.equal(owner(), expected, 'the visible default is also the command owner without changing the select')
      const fields = render()
      fields.find(({ type }) => type === 'Input')?.props.onChange?.({ currentTarget: { value: '检查界面' } })
      fields.find(({ type }) => type === 'Textarea')?.props.onChange?.({ currentTarget: { value: '检查当前界面的操作' } })
      const submit = render().find(({ type }) => type === 'form')?.props.onSubmit
      assert.ok(submit); submit({ preventDefault() {} }); await settleComponent()
      assert.equal(commands.length, 1)
      assert.equal((commands[0] as { owner_bot: string }).owner_bot, expected)
    } finally { app.close() }
  }
})

test('routine input links preserve their exact trigger', () => {
  const shared = parityShared()
  for (const platform of ['web', 'native']) {
    const app = componentHarness(platform === 'web' ? './workspace-chat.tsx' : '../../../../../apps/mobile/src/features/workspace/workspace.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
    })
    try {
      for (const trigger of ['schedule:exact', 'schedule:+/?&中文#%']) {
        const link = app.render(platform === 'web' ? 'InputTargetLink' : 'InputLocation', { conversationId: 'chat', target: { kind: 'routine', bot_id: 'bot', trigger } }).find(({ type }) => type === 'Link')
        assert.ok(link)
        if (platform === 'web') {
          assert.deepEqual(JSON.parse(JSON.stringify(link.props.params)), { botId: 'bot' })
          assert.deepEqual(JSON.parse(JSON.stringify(link.props.search)), { routine: trigger })
        } else {
          assert.equal(typeof link.props.href, 'string')
          const url = new URL(String(link.props.href), 'https://app.vibeface.dev')
          assert.equal(url.pathname, '/b/bot')
          assert.deepEqual([...url.searchParams], [['routine', trigger]])
          assert.equal(url.hash, '')
        }
      }
    } finally { app.close() }
  }
})

test('composers save each pending draft once and let edited drafts save independently', async (t) => {
  const cases = [
    { name: 'web Main', path: '../chat/conversation-screen.tsx', component: 'ConversationScreen', native: false, privateExport: '', props: {} },
    { name: 'web Scoped', path: './workspace-chat.tsx', component: 'ScopedConversation', native: false, privateExport: 'ScopedConversation', props: { target: { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, conversationId: 'chat', channelId: 'channel', label: '项目 Bot' } },
    { name: 'web Work', path: './task-detail.tsx', component: 'TaskDetail', native: false, privateExport: '', props: { botId: 'bot', workId: 'work' } },
    { name: 'native Scoped', path: '../../../../../apps/mobile/src/features/workspace/workspace.tsx', component: 'ScopedWorkspaceScreen', native: true, privateExport: '', props: { channelId: 'channel' } },
    { name: 'native Work', path: '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', component: 'TaskDetail', native: true, privateExport: 'TaskDetail', props: { botId: 'bot', workId: 'work' } },
    { name: 'web Scoped Task', path: './workspace-chat.tsx', component: 'ScopedConversation', native: false, privateExport: 'ScopedConversation', props: { target: { kind: 'task', task_id: 'task' }, conversationId: 'chat', label: '项目 Bot' } },
    { name: 'native Scoped Task', path: '../../../../../apps/mobile/src/features/workspace/workspace.tsx', component: 'ScopedWorkspaceScreen', native: true, privateExport: '', props: { taskId: 'task' } },
  ]
  for (const example of cases) await t.test(example.name, async () => {
    type ReadableFile = { name: string; arrayBuffer: () => Promise<ArrayBuffer> }
    const reads = new Map<string, ReturnType<typeof pendingResult>>()
    const read = async (name: string) => { const held = reads.get(name); assert.ok(held); const buffer = await held.promise; assert.ok(buffer instanceof ArrayBuffer); return buffer }
    const attempts: { text: string; files: string[]; target: InputTarget; intent: string | undefined }[] = []
    const saved: { text: string; attachments: { name: string; bytes: number[] }[] }[] = [], saves: Promise<boolean>[] = []
    const navigations: unknown[] = []
    const domain: WorkspaceDomainState = {
      channels: [{ channel_id: 'channel', title: '频道', purpose: '', members: ['bot'], coordinator_bot: 'bot', coordinator_node: 'worker', revision: 1, owner_epoch: 1, event_hash: 'channel-event', created_at_ms: 1 }],
      tasks: [{ task_id: 'task', root_task_id: 'task', owner_bot: 'bot', title: '任务', request: '交代', expected_result: '结果', source_events: [], dependencies: [], reply_only: false, revision: 1, action: 'continue', request_event: 'request', event_hash: 'task-event', created_at_ms: 1, updated_at_ms: 1, state: 'running' }], assignments: [], inputs: [], coverage: { limit: 128, truncated: false },
    }
    const shared = {
      ...parityShared(), data: parseConversation({ ...context, works: [parityWork()] }), workspaceState: domain,
      chatSlot: { conversationId: 'chat', missing: false }, fresh: new Set<string>(), interrupted: [], view: 'chat',
      readWork: async () => ({ work: { ...parityWork(), conversation_id: 'chat', target: null, source_events: [] }, coverage: { source: 'shared_history', complete: false } }),
      readWorkspaceState: async () => domain, readWorkspaceHistory: async () => ({ events: [], coverage: { limit: 128, truncated: false, scope: 'channel' } }),
      sendMessage: (text: string, files: ReadableFile[], _sent: unknown, options: InputOptions) => {
        attempts.push({ text, files: files.map((file) => file.name), target: options.target, intent: options.intent })
        const taskId = `created-task-${attempts.length}`
        const save = Promise.all(files.map(async (file) => ({ name: file.name, bytes: Array.from(new Uint8Array(await file.arrayBuffer())) }))).then((attachments) => {
          saved.push({ text, attachments })
          if (options.intent === 'new_task') options.onSaved?.({ report_results: true, message_id: `input-${taskId}`, conversation_id: 'chat', target: { kind: 'task', task_id: taskId }, intent: 'continue', expected_revision: 1, sequence: attempts.length, text, attachments: [], created_at_ms: 1, delivery: 'saved' })
          return true
        })
        saves.push(save)
        return save
      },
    }
    const app = componentHarness(example.path, {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { network_id: 'network', can_manage: false, source: 'membership' } } }) },
      '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/workspace/bot-settings.ts': { botCopy, folderLabels, placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage },
      '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters },
      '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) },
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@tanstack/react-router': { useNavigate: () => (target: unknown) => { navigations.push(target) } },
      'expo-router': { Stack: { Screen: 'Screen' }, router: { push: (target: unknown) => { navigations.push(target) } }, useRouter: () => ({ push: (target: unknown) => { navigations.push(target) }, back() {} }) },
      'expo-file-system': { File: class { uri: string; constructor(uri: string) { this.uri = uri } arrayBuffer() { return read(this.uri.split('/').at(-1) ?? '') } } },
      'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) },
      '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_key: string, action: () => Promise<void>) => action() }) },
    }, { setInterval: () => 1, clearInterval() {}, window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, example.privateExport)
    const render = () => app.render(example.component, example.props)
    const composer = () => { const props = render().find(({ type }) => type === (example.native ? 'Composer' : 'PromptInput'))?.props; assert.ok(props); return props }
    const submit = () => { const send = composer()[example.native ? 'onSend' : 'onSubmit']; assert.ok(typeof send === 'function'); send() }
    const attach = (name: string) => {
      const input = composer().onFiles; assert.ok(typeof input === 'function')
      const files = example.native ? [{ uri: `file:///${name}`, name, size: 3, mimeType: 'text/plain' }] : [{ name, arrayBuffer: () => read(name) }]
      input(files)
      return files
    }
    try {
      render(); await settleComponent()
      const first = pendingResult(); reads.set('first.txt', first)
      composer().onChange?.('第一份草稿'); attach('first.txt'); submit(); submit()
      composer().onChange?.('第一份草稿'); submit(); await settleComponent()
      assert.equal(attempts.length, 1, 'unchanged redraws, taps and text events keep the pending draft identity')
      assert.equal(saved.length, 0, 'the first attachment is still preparing before local acceptance')
      const later = pendingResult(); reads.set('later.txt', later)
      composer().onChange?.('第二份草稿'); const laterFiles = attach('later.txt'); submit(); await settleComponent()
      assert.equal(attempts.length, 2, 'editing can submit a new draft while the first attachment is blocked')
      first.resolve(new Uint8Array([1, 2, 3]).buffer); await saves[0]; await settleComponent()
      assert.equal(composer().value, '第二份草稿')
      assert.equal(composer().files, laterFiles, 'the earlier save cannot remove the new attachments')
      later.resolve(new Uint8Array([4, 5, 6]).buffer); await Promise.all(saves); await settleComponent()
      assert.equal(composer().value, '')
      assert.deepEqual(JSON.parse(JSON.stringify(composer().files)), [])
      assert.deepEqual(JSON.parse(JSON.stringify(attempts.map(({ text, files, intent }) => ({ text, files, intent })))), [
        { text: '第一份草稿', files: ['first.txt'], intent: 'continue' }, { text: '第二份草稿', files: ['later.txt'], intent: 'continue' },
      ], 'the composer sends the prompt intent')
      assert.deepEqual(saved, [
        { text: '第一份草稿', attachments: [{ name: 'first.txt', bytes: [1, 2, 3] }] }, { text: '第二份草稿', attachments: [{ name: 'later.txt', bytes: [4, 5, 6] }] },
      ])
      const target: InputTarget = example.name.endsWith('Task') ? { kind: 'task', task_id: 'task' } : example.name === 'web Main' ? { kind: 'bot_main', bot_id: 'bot' } : example.component.includes('Scoped') ? { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' } : { kind: 'work', work_id: 'work' }
      if (example.native) assert.equal(composer().attachmentScope, JSON.stringify(['network', 'chat', target.kind === 'work' ? ['work', target.work_id] : inputTargetKey(target)]))
      assert.deepEqual(JSON.parse(JSON.stringify(attempts.map((item) => item.target))), [target, target])
      const failed = pendingResult(); reads.set('retry.txt', failed)
      composer().onChange?.('原样重试'); const retryFiles = attach('retry.txt'); submit()
      failed.reject(new Error('file unavailable')); await assert.rejects(saves[2], /file unavailable/); await settleComponent()
      assert.equal(composer().value, '原样重试'); assert.equal(composer().files, retryFiles)
      const retry = pendingResult(); reads.set('retry.txt', retry); submit(); submit(); await settleComponent()
      assert.equal(attempts.length, 4, 'a failed save releases the draft guard for exactly one retry')
      retry.resolve(new Uint8Array([7, 8, 9]).buffer); await saves[3]; await settleComponent()
      assert.equal(composer().value, '')
      assert.deepEqual(saved.at(-1), { text: '原样重试', attachments: [{ name: 'retry.txt', bytes: [7, 8, 9] }] })
      if (!example.name.endsWith('Scoped')) for (const change of ['text', 'unchanged']) {
        const before = navigations.length, taskId: string = `created-task-${attempts.length + 1}`, name = `navigate-${change}.txt`
        const held = pendingResult(); reads.set(name, held)
        composer().onChange?.(`新任务 ${change}`); attach(name); submit()
        if (change === 'text') composer().onChange?.('当前正在编辑的正文')
        render()
        held.resolve(new Uint8Array([10, 11, 12]).buffer); await saves.at(-1); await settleComponent()
        assert.equal(navigations.length, before + (change === 'unchanged' ? 1 : 0), 'only the unchanged submitted draft can navigate after local acceptance')
        if (change === 'unchanged') {
          assert.deepEqual(JSON.parse(JSON.stringify(navigations.at(-1))), example.native ? `/task/${taskId}` : { to: '/tasks/$taskId', params: { taskId } })
          assert.equal(composer().value, '')
        } else assert.equal(composer().value, '当前正在编辑的正文', 'late navigation cannot discard the active draft')
      }
    } finally {
      for (const held of reads.values()) held.resolve(new ArrayBuffer(0))
      await Promise.allSettled(saves)
      app.close()
    }
  })
})

test('both channel composers retain the durable task target before the domain snapshot arrives', async () => {
  for (const platform of ['web', 'native']) {
    const domain: WorkspaceDomainState = {
      channels: [{ channel_id: 'channel', title: '官网改版', purpose: '', members: ['bot', 'other'], coordinator_bot: 'bot', coordinator_node: 'worker', revision: 1, owner_epoch: 1, event_hash: 'channel-event', created_at_ms: 1 }],
      tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false },
    }
    const durable: PendingMessage = { report_results: true, message_id: 'locally-saved', conversation_id: 'chat', target: { kind: 'channel_task', channel_id: 'channel', task_id: 'stable-task', bot_id: 'bot' }, intent: 'continue', expected_revision: 1, sequence: 1, text: 'first', created_at_ms: 1, delivery: 'saved' }
    const pendingMessages: PendingMessage[] = []
    const sends: InputOptions[] = []
    const shared = { ...parityShared(), workspaceState: domain, pendingMessages,
      readWorkspaceState: async () => domain,
      readWorkspaceHistory: async () => ({ events: [], coverage: { limit: 128, truncated: false, scope: 'channel' } }),
      sendMessage: async (_text: string, _files: unknown[], _sent: unknown, options?: InputOptions) => { assert.ok(options); sends.push(options); const saved: PendingMessage = options.target.kind === 'channel_member' && options.target.bot_id === 'other' ? { ...durable, conversation_id: 'other-chat', target: { kind: 'channel_task', channel_id: 'channel', task_id: 'other-task', bot_id: 'other' } } : durable; shared.pendingMessages = [saved]; options.onSaved?.(saved); return true },
    }
    shared.bots.push({ ...parityBot, bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' })
    // A cached recipient without its task is ignored: the Channel opens on its coordinator.
    const saved = new Map<string, string>([[`vibeface.${networkKey('channel-target:channel', 'network')}`, JSON.stringify({ bot_id: 'other' })]])
    const storage = { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value) } }
    const dependencies = {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: { principal: { network_id: 'network' } } }) },
      '@/lib/platform': { webPlatform: { storage } }, '@/platform': { mobilePlatform: { storage } },
      'expo-router': { Stack: { Screen: 'Screen' }, router: { push() {} } }, 'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) },
      '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_key: string, action: () => Promise<void>) => action() }) },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    }
    const source = platform === 'web' ? './workspace-chat.tsx' : '../../../../../apps/mobile/src/features/workspace/workspace.tsx'
    const make = () => componentHarness(source, dependencies, { setInterval: () => 1, clearInterval() {} })
    let app = make()
    try {
      if (platform === 'web') {
        const scoped = () => app.render('ChannelWorkspace', { channelId: 'channel' }).find(({ type }) => typeof type === 'function' && type.name === 'ScopedConversation')?.props
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' })
        const accepted = scoped()?.onSaved
        assert.ok(typeof accepted === 'function')
        shared.pendingMessages = [durable]
        accepted(durable)
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), durable.target)
        assert.equal(scoped()?.expectedRevision, 1)
        assert.equal(scoped()?.task, undefined, 'the local target does not invent a server task projection')
        app.close(); app = make()
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), durable.target, 'a reload restores the selected task from durable input evidence')
        assert.equal(scoped()?.expectedRevision, 1)
        const previousSaved = scoped()?.onSaved
        const endReply = app.render('ChannelWorkspace', { channelId: 'channel' }).find(({ type, props }) => type === 'Button' && componentText(props.children) === '结束回复选择')?.props
        assert.ok(typeof endReply?.onClick === 'function'); endReply.onClick()
        scoped()
        assert.ok(typeof previousSaved === 'function'); previousSaved(durable)
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, 'a late save cannot undo a later end of the reply selection')
      } else {
        const composer = () => app.render('ScopedWorkspaceScreen', { channelId: 'channel' }).find(({ type }) => type === 'Composer')?.props
        composer()?.onChange?.('first')
        const first = composer()?.onSend
        assert.ok(typeof first === 'function'); first(); await settleComponent()
        composer()?.onChange?.('second')
        const second = composer()?.onSend
        assert.ok(typeof second === 'function'); second(); await settleComponent()
        assert.deepEqual(sends.map((input) => JSON.parse(JSON.stringify(input.target))), [{ kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, durable.target])
        assert.equal(sends[1].expected_revision, 1)
        app.close(); app = make()
        composer()?.onChange?.('after reload')
        const afterReload = composer()?.onSend
        assert.ok(typeof afterReload === 'function'); afterReload(); await settleComponent()
        assert.deepEqual(JSON.parse(JSON.stringify(sends[2].target)), durable.target)
        assert.equal(sends[2].expected_revision, 1)
        const endReply = app.render('ScopedWorkspaceScreen', { channelId: 'channel' }).find(({ props }) => props.accessibilityLabel === '结束回复这个任务')?.props
        assert.ok(typeof endReply?.onPress === 'function'); endReply.onPress()
        composer()
        sends[2].onSaved?.(durable)
        composer()?.onChange?.('交给协调 Bot')
        const sendCoordinator = composer()?.onSend
        assert.ok(typeof sendCoordinator === 'function'); sendCoordinator(); await settleComponent()
        assert.deepEqual(JSON.parse(JSON.stringify(sends[3].target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, 'a late save cannot redirect the next native input back to the old task')
      }
    } finally { app.close() }
  }
})

test('chat task groups stay at their originating message and orphan tasks remain available only in history', () => {
  const messages = [
    { message_id: 'one', event_hash: 'one', role: 'user', text: '核对项目', created_at_ms: 1 },
    { message_id: 'reply', event_hash: 'reply', role: 'assistant', text: '已开始核对', created_at_ms: 2, in_reply_to: 'one' },
    { message_id: 'two', event_hash: 'two', role: 'user', text: '继续', created_at_ms: 3 },
  ]
  const complete = (work_id: string, trigger = 'message:one') => parityWork({ work_id, trigger, state: 'finished', outcome: 'completed', title: `已完成的 ${work_id}`, result_text: '不在聊天重复的结果' })
  const shared = { ...parityShared(), fresh: new Set<string>(), interrupted: [], chatSlot: { conversationId: 'chat', missing: false } }
  shared.data = parseConversation({ ...context, messages, works: [complete('root'), complete('sibling'), parityWork({ work_id: 'child', trigger: 'task:root' }), complete('orphan', 'message:missing')] })
  const harness = conversationHarness(() => shared)
  try {
    const entries = harness.chatEntries()
    assert.deepEqual(entries.map((entry) => entry.key), ['one', 'task-group:message:one', 'task-group:task:root', 'reply', 'two'])
    const group = entries.find((entry) => entry.key === 'task-group:message:one')?.node
    assert.equal(group?.type, 'Details')
    assert.equal(group?.props.label, '2 个任务 · 已完成')
    const elements = harness.render()
    const cards = elements.filter(({ type }) => typeof type === 'function' && type.name === 'TaskCard')
    assert.equal(cards.length, 3, 'the orphan cannot create a chat row')
    const finished = cards.find(({ props }) => (props.task as { id: string }).id === 'sibling')
    assert.ok(finished)
    assert.equal(componentText(nestedElements([finished], ['TaskCard']).find(({ type }) => type === 'div')), '已完成 · 查看任务')
    const history = elements.find(({ type }) => typeof type === 'function' && type.name === 'TaskHistory')
    assert.equal(history, undefined, 'an inactive chat never mounts a history portal')
    const active = harness.render({ active: true })
    const historyKey = active.find(({ type }) => typeof type === 'function' && type.name === 'TaskHistory')?.key
    const chatKey = active.find(({ type }) => type === 'Chat')?.key
    assert.ok(historyKey && chatKey)
    assert.notEqual(historyKey, chatKey, 'history and the retained chat are different siblings during reconciliation')
    shared.data = parseConversation({ ...context, messages: messages.slice(0, 1), works: shared.data.works })
    assert.equal(harness.chatEntries()[1].key, 'task-group:message:one', 'a reply arriving never moves the task group')
    shared.data = parseConversation({ ...context, messages, works: [complete('root'), complete('sibling'), complete('child', 'task:root'), complete('orphan', 'message:missing')] })
    assert.equal(harness.chatEntries()[2].key, 'task-group:task:root', 'progress changes retain the group key')
  } finally { harness.app.close() }
})

test('chat task history keeps completed and unknown records while counting only current attention', () => {
  const approval = { question_id: 'approval', conversation_id: 'chat', target: { kind: 'bot_main', bot_id: 'bot' }, attention: { kind: 'approval', device_id: 'worker' }, severity: 'attention', text: '允许继续吗？', choices: [{ label: '继续' }], author: 'worker', created_at_ms: 2, answer: null, closed: null, about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' } }
  const failed = parityWork({ work_id: 'failed', state: 'finished', outcome: 'failed', failure_reason: '已保留修改' })
  const openWorks: (WorkRecord & { conversation_id: string })[] = []
  const shared = { ...parityShared(), openWorks, fresh: new Set<string>(), interrupted: [], chatSlot: { conversationId: 'chat', missing: false } }
  shared.data = parseConversation({ ...context, works: [parityWork(), failed, parityWork({ work_id: 'unknown', state: 'unverified', outcome: 'unverified', failure_reason: '结果无法确认' }), parityWork({ work_id: 'completed', state: 'finished', outcome: 'completed' })], questions: [approval] })
  shared.openWorks = shared.data.works.map((work) => ({ ...work, conversation_id: 'chat' }))
  const dependencies = {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels, '@/lib/glossary': { terms: { approval: { icon: 'ApprovalIcon' } } },
  }
  const app = componentHarness('../chat/conversation-screen.tsx', dependencies, {}, 'TaskHistory, TaskIcon')
  const harness = conversationHarness(() => shared)
  const render = () => {
    const history = harness.render({ active: true }).find(({ type }) => typeof type === 'function' && type.name === 'TaskHistory')
    assert.ok(history)
    return app.render('TaskHistory', history.props)
  }
  const label = () => render().find(({ type }) => type === 'Button')?.props['aria-label']
  try {
    assert.equal(label(), '任务 · 3 待处理')
    assert.equal(render().filter(({ type }) => type === 'Link').length, 4, 'history includes completed, missing-origin and uncertain tasks')
    assert.match(componentText(render()), /已完成/)
    assert.match(componentText(render()), /待确认/)
    assert.equal(app.render('TaskIcon', { state: 'unknown', stale: false }).some(({ type }) => type === 'CircleQuestionMark'), true)
    assert.equal(app.render('TaskIcon', { state: 'completed', stale: false }).some(({ type }) => type === 'CircleCheck'), true)
    shared.answered = { approval: '继续' }
    shared.openWorks = shared.openWorks.filter((work) => work.work_id !== 'failed')
    assert.equal(label(), '任务 · 1 进行中 · 1 待处理', 'answered approvals and closed historical failures stop counting')
    const trigger = render().find(({ type }) => type === 'Button')
    trigger?.props.onClick?.()
    assert.equal(render().find(({ type }) => type === 'Sheet')?.props.open, true)
    const onCloseAutoFocus = render().find(({ type }) => type === 'SheetContent')?.props.onCloseAutoFocus as (event: { preventDefault(): void }) => void
    render().find(({ type }) => type === 'Link')?.props.onClick?.()
    assert.equal(render().find(({ type }) => type === 'Sheet')?.props.open, false, 'navigation closes the sheet first')
    let prevented = false
    const closeFocus = () => onCloseAutoFocus({ preventDefault() { prevented = true } })
    closeFocus()
    assert.equal(prevented, true, 'detail navigation owns focus after the sheet closes')
    render().find(({ type }) => type === 'Button')?.props.onClick?.()
    prevented = false
    closeFocus()
    assert.equal(prevented, false, 'ordinary dismissal still restores the opening button')
    shared.connectionOnline = false
    assert.equal(label(), '任务 · 2 待处理', 'offline active tasks are uncertain instead of counted as running')
  } finally { app.close(); harness.app.close() }
})

test('the retained conversation only mounts task history on its exact chat route', () => {
  let pathname = '/b/bot'
  const shared = { ...parityShared(), readWorkspaceState: async () => workspaceModel.parseWorkspaceState({ coverage: { limit: 128, truncated: false } }) }
  const app = componentHarness('./workspace-shell.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: null, directory: null }) },
    '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {}, useRouterState: ({ select }: { select: (state: unknown) => unknown }) => select({ location: { pathname, search: {} } }) },
    '@/components/ui/sidebar': { useSidebar: () => ({ isMobile: false, setOpenMobile() {} }) }, './inbox': { useInbox: () => [] },
    '@/features/devices/join': { useOpenJoins: () => [] }, '@vibeface/client/workspace/inbox.ts': { waitingItems: () => [] },
    '@vibeface/client/workspace/home.ts': { backTo: () => undefined }, '@/lib/glossary': { terms: Object.fromEntries(['inbox', 'task', 'myDevices', 'settings'].map((key) => [key, { label: key, icon: key }])) },
    '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle' }, '@vibeface/client/avatar/mood.ts': { moodLabel: { idle: '待命' } },
  }, {}, 'WorkspaceLayout')
  const active = () => app.render('WorkspaceLayout', { children: null, landing: false }).find(({ type }) => type === 'ConversationScreen')?.props.active
  try {
    assert.equal(active(), true)
    for (const route of ['/b/bot/work/work', '/devices', '/tasks', '/b/missing']) { pathname = route; assert.equal(active(), false, route) }
    pathname = '/b/bot'
    assert.equal(active(), true)
  } finally { app.close() }
})

test('closing task detail restores a connected opener or the persistent workspace after history unmounts', () => {
  const focused: string[] = []
  class Target {
    isConnected = true
    readonly name: string
    constructor(name: string) { this.name = name }
    focus(options: { preventScroll: boolean }) { assert.equal(options.preventScroll, true); focused.push(this.name) }
  }
  const body = new Target('body'), main = new Target('main'), link = new Target('history-link')
  const ownerDocument = { activeElement: link, body, getElementById: (id: string) => { assert.equal(id, 'main'); return main } }
  const panel = { ownerDocument, focus: (options: { preventScroll: boolean }) => { assert.equal(options.preventScroll, true); focused.push('detail') } }
  const app = componentHarness('./task-detail.tsx', {}, { HTMLElement: Target }, 'holdFocus')
  try {
    for (const source of ['connected', 'removed', 'body']) {
      ownerDocument.activeElement = source === 'body' ? body : link
      link.isConnected = source === 'connected'
      app.render('holdFocus', panel)
      const close = app.output
      assert.equal(typeof close, 'function')
      if (typeof close === 'function') close()
    }
    assert.deepEqual(focused, ['detail', 'history-link', 'detail', 'main', 'detail', 'main'])
  } finally { app.close() }
})

const taskOverviewUi = {
  '@vibeface/client/lib/errors.ts': errors,
  '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
  '@/components/kit': { providerIcons: { codex: 'CodexIcon' } },
}
function taskOverviewHarness(
  platform: string, shared: ReturnType<typeof parityShared>, dependencies: Record<string, unknown>,
  inbox: () => unknown[] = () => [],
) {
  const app = componentHarness(platform === 'web' ? './tasks-overview.tsx' : '../../../../../apps/mobile/src/features/home/tasks.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared, useInbox: inbox },
    '@/data/context': { useWorkspace: () => ({ bots: shared.bots }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    './inbox': { useInbox: inbox }, './workspace-shell': { useCreateBot: () => () => {} },
    ...dependencies,
  })
  return {
    app,
    render: () => app.render(platform === 'web' ? 'TasksOverview' : 'TasksScreen'),
    cardName: platform === 'web' ? 'TaskOverviewCard' : 'TaskRow',
    mastheadName: platform === 'web' ? 'WorkspaceMasthead' : 'MobileMasthead',
  }
}

test('both task overviews preserve known records, show their coverage and only navigate cross-Bot program checks', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    shared.openWorks = Array.from({ length: 257 }, (_, index) => ({ ...parityWork({ work_id: `work-${index}`, prompt: `完整请求 ${index}` }), conversation_id: 'chat' }))
    shared.overview = workspaceModel.parseWorkspaceOverview({
      bots: [parityBot, { bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' }],
      tasks: shared.openWorks.slice(0, 256), truncated: true, consistency: 'partial', checks: [{ check_id: 'other-process', conversation_id: 'other-chat', trigger: 'message:remote' }],
      coverage: { source: 'workspace_overview', truncated: true, omitted_tasks: 1, omitted_questions: 2, remote_owners: [{ owner: 'remote', reason: 'remote_scheduler_not_queried' }] },
    })
    const { app, render, cardName, mastheadName } = taskOverviewHarness(platform, shared, taskOverviewUi)
    try {
      let elements = render()
      const cards = elements.filter(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.equal(cards.length, 257, `${platform} keeps the row outside the bounded overview`)
      assert.equal((cards[256].props.work as WorkRecord).prompt, '完整请求 256')
      assert.match(componentText(elements), /总览已截断/)
      assert.match(componentText(elements), /本次总览未返回 1 项任务/)
      assert.match(componentText(elements), /本次总览未返回 2 项问题/)
      assert.match(componentText(elements), /尚未读取其他设备的后台工作/)
      assert.doesNotMatch(componentText(elements), /未返回 0 项/)
      assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, 'another Bot program check cannot settle against the active conversation')
      assert.equal(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard']).some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === 'other' : props.href === '/b/other')), true)
      shared.nodes = [{ ...parityNode, online: false }]
      elements = render()
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 258)
      const offlineCard = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.ok(offlineCard && typeof offlineCard.type === 'function')
      assert.match(componentText(offlineCard.type(offlineCard.props)), /待确认/)
      shared.nodes = [parityNode]
      shared.connectionOnline = false
      elements = render()
      const staleCard = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.ok(staleCard && typeof staleCard.type === 'function')
      assert.match(componentText(staleCard.type(staleCard.props)), /状态未知/)
      shared.synced = false
      shared.openWorks = []
      shared.overview = workspaceModel.parseWorkspaceOverview({})
      elements = render()
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.working, null)
      assert.doesNotMatch(componentText(elements), /没有未完成的任务/)
      shared.synced = true
      shared.connectionOnline = true
      elements = render()
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.working, 0)
      const empty = elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'WorkspaceEmpty' : 'MobileEmpty'))
      assert.ok(empty && typeof empty.type === 'function')
      assert.match(platform === 'web' ? String(empty.props.title) : componentText(empty.type(empty.props)), /当前记录没有未完成的任务/)
      if (platform === 'native') {
        assert.equal(elements.some(({ type, props }) => type === 'Link' && props.href === '/inbox'), true, 'the incoming mobile inbox entry remains available with no pending items')
        assert.equal(elements.some(({ props }) => props.accessibilityLabel === '查看待处理'), true)
      }
    } finally { app.close() }
  }
})

test('both task overviews recover bounded Bot identities only from the current complete catalog', () => {
  for (const platform of ['web', 'native']) {
    const bots = Array.from({ length: 257 }, (_, index) => ({ ...parityBot, bot_id: `bot-${index + 1}`, conversation_id: `chat-${index + 1}`, name: `项目 Bot ${index + 1}` }))
    const shared = { ...parityShared(), conversationId: 'chat-1', bots, openWorks: [], overview: workspaceModel.parseWorkspaceOverview({ bots: bots.slice(0, 256), checks: [{ check_id: 'only-check', conversation_id: 'chat-257', trigger: 'message:one' }], truncated: true }) }
    const { app, render } = taskOverviewHarness(platform, shared, {
      '@/components/kit': { providerIcons: {} },
    })
    const opens = (elements: ReturnType<typeof render>, botId = 'bot-257') => nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard']).some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === botId : props.href === `/b/${botId}`))
    try {
      let elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /项目 Bot 257/)
      assert.equal(opens(elements), true, `${platform} keeps the only navigation for the Bot outside overview.bots`)
      assert.doesNotMatch(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /已删除的 Bot/)
      shared.bots = bots.slice(0, 256)
      elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /未找到所属 Bot/)
      assert.equal(opens(elements), false)
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /程序状态待确认/, 'a missing identity does not remove the check')
      shared.bots = [{ ...bots[256], conversation_id: 'different-chat' }]
      assert.equal(opens(render()), false, 'a Bot from a different conversation cannot supply navigation')
      shared.bots = bots
      shared.synced = false
      shared.generation++
      elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /Bot 身份未加载/)
      assert.doesNotMatch(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /项目 Bot 257|已删除的 Bot/)
      assert.equal(opens(elements), false, 'a retained catalog before this connection first read cannot supply an old network identity')
      shared.bots = [{ ...bots[256], bot_id: 'new-network-bot', name: '新网络 Bot' }]
      shared.synced = true
      elements = render()
      assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /新网络 Bot/)
      assert.equal(opens(elements, 'new-network-bot'), true)
      assert.equal(opens(elements), false)
    } finally { app.close() }
  }
})

test('both task overviews retain failure outcomes and causes across unordered snapshots', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const { app, render, cardName, mastheadName } = taskOverviewHarness(platform, shared, taskOverviewUi)
    try {
      for (const observedFailure of [true, false]) {
        const running = { ...parityWork({ summary: '', result_text: '完整输出', failure_reason: '旧快照记录的说明' }), conversation_id: 'chat' }
        const failed = { ...parityWork({ ...running, state: 'finished', outcome: 'failed', failure_reason: '这次执行失败，已保留改动' }), conversation_id: running.conversation_id }
        shared.openWorks = [observedFailure ? running : failed]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...(observedFailure ? failed : running), kind: 'work' }] })
        const elements = render()
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
        assert.ok(card && typeof card.type === 'function')
        const text = componentText(card.type(card.props))
        assert.match(text, /失败/, `${platform} uses the outcome rather than treating finished as success`)
        assert.match(text, /这次执行失败，已保留改动/)
        assert.doesNotMatch(text, /旧快照记录的说明/)
        assert.doesNotMatch(text, /已完成/)
        assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 1)
        assert.equal((card.props.work as WorkRecord).result_text, '完整输出')
      }
      for (const [known, observed] of [['completed', 'unverified'], ['unverified', 'completed'], ['completed', 'failed'], ['failed', 'completed']] as const) {
        shared.openWorks = [{ ...parityWork({ state: known === 'unverified' ? 'unverified' : 'finished', outcome: known, summary: '' }), conversation_id: 'chat' }]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...parityWork({ state: observed === 'unverified' ? 'unverified' : 'finished', outcome: observed }), conversation_id: 'chat', kind: 'work' }] })
        const elements = render()
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
        assert.ok(card && typeof card.type === 'function')
        const text = componentText(card.type(card.props))
        assert.match(text, /待确认|状态未知/)
        assert.doesNotMatch(text, /已完成/)
        assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 1)
      }
    } finally { app.close() }
  }
})

test('both task overviews reconcile raw confirmations and only fold task process checks on the same device', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    shared.conversationId = 'other-chat'
    const entry = { ...parityNode, device_id: 'entry', local: true }
    const inbox = () => inboxItems({ device: { ready: null, joins: [], pendingRecovery: null, rotateHint: false }, workspace: shared })
    const { app, render, cardName, mastheadName } = taskOverviewHarness(platform, shared, taskOverviewUi, inbox)
    const count = (elements: ReturnType<typeof render>) => elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention
    try {
      for (const unreachable of ['offline', 'disconnected']) {
        shared.nodes = [entry, { ...parityNode, online: unreachable !== 'offline', capabilities: { ...parityNode.capabilities, codex_connected: unreachable !== 'disconnected' } }]
        shared.openWorks = [{ ...parityWork({ summary: '' }), conversation_id: 'chat' }]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...shared.openWorks[0], kind: 'work', state: 'finished', outcome: 'failed', failure_reason: '总览记录的失败原因' }] })
        const elements = render()
        assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, `${platform} cannot offer settlement for an already failed work`)
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
        assert.ok(card && typeof card.type === 'function')
        assert.match(componentText(card.type(card.props)), /失败.*总览记录的失败原因/)
        assert.equal(count(elements), 1)
      }
      const unknown = { ...parityWork({ kind: 'task', state: 'unverified', outcome: 'unverified', target_node: 'entry' }), conversation_id: 'chat' }
      const checks = [{ check_id: 'process-work', conversation_id: 'chat', trigger: 'task:work' }]
      for (const variant of ['same', 'outside-bound', 'missing', 'other-conversation', 'other-device', 'running']) {
        const known = variant === 'other-conversation' ? { ...unknown, conversation_id: 'different-chat' } : variant === 'other-device' ? { ...unknown, target_node: 'worker' } : variant === 'running' ? { ...parityWork({ ...unknown, state: 'running', outcome: null }), conversation_id: unknown.conversation_id } : unknown
        shared.nodes = [entry, parityNode]
        shared.openWorks = variant === 'missing' ? [] : [known]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: variant === 'outside-bound' || variant === 'missing' ? [] : [{ ...known, kind: 'task' }], checks })
        const elements = render()
        assert.equal(count(elements), ['same', 'outside-bound', 'missing'].includes(variant) ? 1 : variant === 'running' ? 1 : 2, `${platform} ${variant} preserves exactly the represented checks`)
        if (variant === 'same' || variant === 'outside-bound') {
          if (platform === 'web') assert.equal(elements.filter(({ type }) => type === 'ConfirmCard').length, 1, 'the real unknown work keeps its confirmation action')
          assert.doesNotMatch(componentText(elements), /程序状态待确认|任务状态待确认/)
        }
        if (variant === 'missing') {
          assert.match(componentText(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard'])), /任务状态待确认/)
          assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, 'a bounded-out work keeps navigation rather than a settlement against the active Bot')
          assert.equal(elements.some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === 'bot' : props.href === '/b/bot')), true)
        }
      }
    } finally { app.close() }
  }
})

test('both work details read terminal shared records outside Main and discard late route responses', async () => {
  for (const platform of ['web', 'native']) {
    const first = pendingResult(), second = pendingResult()
    const calls: { conversation_id: string; work_id: string }[] = []
    const shared = { ...parityShared(), openWorks: [], readWork: (input: { conversation_id: string; work_id: string }) => { calls.push(input); return input.work_id === 'first' ? first.promise : second.promise } }
    shared.data = parseConversation(context)
    let workId = 'first'
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, platform === 'native' ? 'TaskDetail' : '')
    const render = () => app.render('TaskDetail', { botId: 'bot', workId })
    const content = () => { const elements = render(); return componentText(elements) + elements.map(({ props }) => typeof props.text === 'string' ? props.text : '').join('') }
    try {
      render()
      workId = 'second'
      render()
      second.resolve({ work: { ...parityWork({ work_id: 'second', title: 'Task 内已结束工作', state: 'finished', outcome: 'completed', result_text: '完整结果记录' }), target: null, source_events: ['a'.repeat(64)], conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } })
      await settleComponent()
      assert.deepEqual(JSON.parse(JSON.stringify(calls)), [{ conversation_id: 'chat', work_id: 'first' }, { conversation_id: 'chat', work_id: 'second' }])
      assert.match(content(), /完整结果记录/)
      assert.match(content(), /完整执行历史未提供/)
      first.resolve({ work: { ...parityWork({ work_id: 'first', title: '旧路由迟到内容' }), target: null, source_events: ['b'.repeat(64)], conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } })
      await settleComponent()
      assert.match(content(), /完整结果记录/)
      assert.doesNotMatch(content(), /旧路由迟到内容/)
      assert.equal(shared.data.works.length, 0, 'detail reads never refill Main')
      assert.equal(shared.openWorks.length, 0, 'terminal history does not depend on the open-work list')
    } finally { app.close() }
  }
})

test('terminal work followups retain their Task identity or explicitly create a task from the original request', async () => {
  for (const platform of ['web', 'native']) for (const kind of ['task', 'channel_task', 'raw', 'unverified']) {
    const target: InputTarget | null = kind === 'task' ? { kind: 'task', task_id: 'stable-task' } : kind === 'channel_task' ? { kind: 'channel_task', task_id: 'stable-task', channel_id: 'channel', bot_id: 'bot' } : null
    const work = kind === 'unverified' ? parityWork({ state: 'unverified' }) : parityWork({ state: 'finished', outcome: 'failed', prompt: '原始完整交代', result_text: '上次已经产生的输出' })
    const sends: { text: string; options: InputOptions }[] = [], reads: unknown[] = []
    const shared = { ...parityShared(), data: parseConversation(context), openWorks: [],
      readWork: async () => ({ work: { ...work, target, source_events: ['a'.repeat(64)], conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } }),
      readWorkspaceState: async (filter: unknown) => { reads.push(filter); return { tasks: [{ task_id: 'stable-task', revision: 3, action: 'continue' }] } },
      sendMessage: async (text: string, _files: unknown[], _sent: unknown, options: InputOptions) => { sends.push({ text, options }); return true },
    }
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, action: () => Promise<void>) => action() }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, platform === 'native' ? 'TaskDetail' : '')
    const render = () => app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    try {
      render(); await settleComponent()
      const elements = render()
      const composer = elements.find(({ type }) => type === (platform === 'web' ? 'PromptInput' : 'Composer'))
      assert.ok(composer)
      if (kind === 'unverified') {
        assert.equal(platform === 'web' ? composer.props.disabled : Boolean(composer.props.blocked), true, 'an unfinished Work that cannot take a supplement blocks its input')
        assert.equal(platform === 'web' ? composer.props.reason : composer.props.blocked, '这个工作当前不能接收补充。')
        continue
      }
      assert.ok(!(platform === 'web' ? composer.props.disabled : composer.props.blocked), 'a terminal Work still accepts a followup')
      assert.equal(composer.props.label, '发送为新任务')
      assert.equal(composer.props.sendLabel, '发送为新任务')
      const follow = elements.find(({ type, props }) => type === (platform === 'web' ? 'Button' : 'ActionButton') && componentText(props.children) === (target ? '继续所属任务' : '新任务重做'))
      assert.ok(follow)
      if (platform === 'web') { assert.ok(follow.props.onClick); follow.props.onClick(); await settleComponent() }
      else { assert.ok(follow.props.action); await follow.props.action() }
      assert.equal(sends.length, 1)
      assert.deepEqual(JSON.parse(JSON.stringify(sends[0].options.target)), target ?? { kind: 'work', work_id: 'work' })
      assert.equal(sends[0].options.intent, target ? 'continue' : 'new_task')
      assert.equal(sends[0].options.expected_revision, target ? 3 : undefined)
      assert.match(sends[0].text, /原始完整交代/)
      assert.match(sends[0].text, /上次已经产生的输出/)
      assert.deepEqual(JSON.parse(JSON.stringify(reads)), target ? [{ task_id: 'stable-task' }] : [])
      const prompt = render().find(({ type }) => type === (platform === 'web' ? 'PromptInput' : 'Composer'))?.props
      prompt?.onChange?.('接着改')
      const submit = render().find(({ type }) => type === (platform === 'web' ? 'PromptInput' : 'Composer'))?.props[platform === 'web' ? 'onSubmit' : 'onSend']
      assert.ok(typeof submit === 'function'); submit(); await settleComponent()
      assert.equal(sends.length, 2)
      assert.equal(sends[1].text, '接着改')
      assert.deepEqual(JSON.parse(JSON.stringify(sends[1].options.target)), { kind: 'work', work_id: 'work' })
      assert.equal(sends[1].options.intent, 'new_task', 'a terminal Work followup is sent as a new task')
    } finally { app.close() }
  }
})

test('both task details bind approval to current unfinished work and keep stops above approval', async () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const approval = { target: { kind: 'work', work_id: 'work' }, question_id: 'approval', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '继续吗', choices: [], author: 'worker', created_at_ms: 2, answer: null, closed: null, about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' } }
    shared.questions = workspaceModel.parseQuestions({ questions: [approval] })
    shared.data = parseConversation({ ...context, works: [parityWork({ work_id: 'old', state: 'finished', outcome: 'completed' }), parityWork()], questions: [approval] })
    let workId = 'old'
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }), useLocalSearchParams: () => ({ botId: 'bot', workId }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, platform === 'native' ? 'TaskDetail' : '')
    const render = async () => { app.render('TaskDetail', { botId: 'bot', workId }); await settleComponent(); return app.render('TaskDetail', { botId: 'bot', workId }) }
    try {
      assert.equal((await render()).some(({ type }) => type === 'QuestionCard'), false, `${platform} old work cannot inherit the later approval`)
      workId = 'work'
      let elements = await render()
      assert.equal(elements.some(({ type }) => type === 'QuestionCard'), true)
      assert.match(componentText(elements), /需要你同意/)
      assert.equal(elements.some(({ props }) => props.label === '最近步骤摘要 · 3'), true)
      shared.answered = { approval: '继续' }
      assert.equal((await render()).some(({ type }) => type === 'QuestionCard'), false)
      shared.answered = {}
      shared.data = parseConversation({ ...context, works: [parityWork()], deliveries: [{ work_id: 'work', target_node: 'worker', state: 'stop_waiting' }], questions: [approval] })
      elements = await render()
      assert.match(componentText(elements), /停止中/)
      assert.doesNotMatch(componentText(elements), /需要你同意/)
      shared.nodes = [{ ...parityNode, capabilities: { ...parityNode.capabilities, codex_connected: false } }]
      assert.match(componentText(await render()), /待确认/)
      shared.nodes = [parityNode]
      shared.data = parseConversation({ ...context, works: [parityWork({ kind: 'task', coordination_job: { conversation_id: 'chat', scope: { kind: 'task', work_id: 'work' }, trigger: 'task:work', kind: 'workspace_task', state: 'queued', sequence: 1, lane: 'task', source_node: 'worker', owner: 'worker', message_id: null, reply_message_id: 'reply', hold_reason: 'machine_slot' } })] })
      elements = await render()
      assert.equal(elements.some(({ props }) => props.label === '协调回合'), true)
      assert.match(componentText(elements), /等待设备运行名额/)
      assert.equal(shared.data.works[0].state, 'running', 'queue observations leave DAG work state intact')
    } finally { app.close() }
  }
})

test('context shows scheduler observations without promising native resume or repeating external sessions', () => {
  const shared = parityShared()
  const data = parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:queued', state: 'queued' }], unanswered: [] } })
  const dependencies = { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/chat/protocol.ts': { promptIntent, runtimeLabel }, '@vibeface/client/workspace/bot-settings.ts': { scheduleFrequency: () => '一次', formatWhen: () => '刚才' }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } }
  const app = componentHarness('./context-panel.tsx', dependencies)
  try {
    const elements = app.render('ContextPanel', { bot: parityBot, data, nodes: shared.nodes, connected: true, onBack() {} })
    assert.equal(elements.filter(({ type }) => type === 'CollaborationPanel').length, 1)
    const continuity = elements.find(({ type }) => typeof type === 'function' && type.name === 'AgentContinuity')
    assert.ok(continuity && typeof continuity.type === 'function')
    let tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /排队中/)
    assert.doesNotMatch(componentText(tree), /正在运行|设备默认模型 · 设备默认模型/)
    data.scheduler!.jobs = [{ scope: { kind: 'main' as const }, trigger: 'message:stop', state: 'cancelling' }]
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /停止中/)
    data.scheduler!.jobs = [{ scope: { kind: 'main' as const }, trigger: 'message:publish', state: 'publishing' }]
    assert.match(componentText(continuity.type(continuity.props)), /回复同步中/)
    data.scheduler!.provider = 'claude'
    data.scheduler!.model = 'new-device-default'
    data.scheduler!.jobs = [{ scope: { kind: 'main' as const }, trigger: 'message:read-back', state: 'reconciling', runtime: workspaceModel.parseTurnRuntime({ provider: 'codex', model: 'frozen-model', reasoning_effort: 'high', generation: 'config', session_generation: 1, delivery_id: 'delivery', stage: 'read_back', execution: 'unknown', attempted_at_ms: 1, switched_from: null, switch_reason: null }) }]
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /正在查询上次回复/)
    assert.match(componentText(tree), /Codex · 提交模型 frozen-model/)
    assert.doesNotMatch(componentText(tree), /new-device-default|Claude Code/)
    data.scheduler = parseConversation({ ...context, scheduler: { scope_jobs: [], provider: 'codex', model: 'current-model', unanswered: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:reconcile', state: 'reconciling', runtime: { provider: 'claude', model: 'frozen-model', reasoning_effort: 'high', generation: 'generation', session_generation: 1, delivery_id: 'delivery', stage: 'read_back', execution: 'unknown', attempted_at_ms: 1, switched_from: null, switch_reason: null } }] } }).scheduler
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /正在查询上次回复/)
    assert.match(componentText(tree), /Claude Code.*frozen-model/)
    assert.doesNotMatch(componentText(tree), /current-model/)
    const runtime = data.scheduler?.jobs[0]?.runtime
    assert.ok(runtime)
    for (const state of ['publishing', 'pending_delivery']) {
      data.scheduler = parseConversation({ ...context, scheduler: { scope_jobs: [], unanswered: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:history', state: 'completed', runtime: { ...runtime, model: 'newer-history', attempted_at_ms: 2 } }, { scope: { kind: 'main' as const }, trigger: 'message:active', state, runtime }] } }).scheduler
      tree = continuity.type(continuity.props)
      assert.match(componentText(tree), /Claude Code.*frozen-model/)
      assert.doesNotMatch(componentText(tree), /newer-history/)
    }
    data.scheduler = null
    assert.match(componentText(continuity.type(continuity.props)), /后台工作状态未提供/)
    assert.equal(elements.some(({ type, props }) => typeof type === 'function' && type.name === 'ContinuityStat' && props.label === '后台工作' && props.value === null), true)
  } finally { app.close() }

  const collaboration = componentHarness('./collaboration-panel.tsx', { ...dependencies,
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/lib/node.ts': { providerNames }, '@/features/devices/ai-settings': { providerIcons: { codex: 'CodexIcon' } },
  })
  const records = parseConversation({ ...context,
    scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:q', state: 'queued' }, { scope: { kind: 'main' as const }, trigger: 'message:p', state: 'publishing' }, { scope: { kind: 'main' as const }, trigger: 'message:u', state: 'future' }, { scope: { kind: 'main' as const }, trigger: 'message:d', state: 'completed' }], unanswered: [] },
    sessions: [{ node: 'worker', agent: 'codex', session_id: 'external', cwd: '/shared', summary: '外部摘要', muted: true, created_at_ms: 1 }],
    sent: [{ message_id: 'sent', to: 'chat', text: '协作请求', created_at_ms: 2 }],
  })
  try {
    const elements = collaboration.render('CollaborationPanel', { data: records, botId: 'bot' })
    assert.equal(elements.find(({ type, props }) => typeof type === 'function' && type.name === 'Section' && props.title === '未结束的后台工作')?.props.count, 3)
    assert.equal(elements.some(({ type, props }) => type === 'Details' && props.label === '后台工作历史 · 1'), true)
    const publishing = elements.find(({ type, props }) => typeof type === 'function' && type.name === 'JobRow' && (props.job as { state: string }).state === 'publishing')
    assert.ok(publishing && typeof publishing.type === 'function')
    assert.match(componentText(publishing.type(publishing.props)), /回复同步中/)
    const sessions = elements.filter(({ type }) => typeof type === 'function' && type.name === 'SessionRows')
    assert.equal(sessions.length, 1)
    assert.ok(typeof sessions[0].type === 'function')
    shared.nodes = [{ ...parityNode, revoked: true }]
    const sessionTree = sessions[0].type({ ...sessions[0].props, nodes: shared.nodes })
    assert.match(componentText(sessionTree), /已静音/)
    assert.match(componentText(sessionTree), /设备已撤权/)
    const messages = elements.find(({ type }) => typeof type === 'function' && type.name === 'CollaborationMessages')
    assert.ok(messages && typeof messages.type === 'function')
    messages.type(messages.props)
    const sent = elements.find(({ type }) => typeof type === 'function' && type.name === 'SentMessage')
    assert.ok(sent && typeof sent.type === 'function')
    const sentTree = sent.type(sent.props)
    assert.match(componentText(sentTree), /对方是否收到或回复尚不清楚/)
    assert.equal(elements.some(({ type, props }) => type === 'Link' && (props.params as { botId?: string })?.botId === 'bot'), true)
  } finally { collaboration.close() }
})

test('native context and task detail keep selection errors and foreign payloads bound to their route and connection', async () => {
  for (const route of ['context', 'work']) {
    let generation = 1
    let shared = parityShared()
    const old = pendingResult(), next = pendingResult()
    const views: string[] = []
    shared.conversationId = 'other-chat'
    shared.data = parseConversation({ ...context, conversation: { ...context.conversation, conversation_id: 'other-chat' }, works: [parityWork({ title: '别的 Bot 的秘密记录' })] })
    shared.synced = false
    shared.isCurrent = () => generation === 1
    shared.selectBot = () => old.promise.then(() => {})
    shared.setView = (view) => { views.push(view) }
    const app = componentHarness(route === 'context' ? '../../../../../apps/mobile/app/b/[botId]/context.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot', workId: 'work' }), useRouter: () => ({ push() {}, back() {} }) },
      '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/lib/errors.ts': errors, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@/components/kit': { EmptyState: 'EmptyState', Details: 'Details', providerIcons: { codex: 'CodexIcon' }, toneText: {} },
    }, {}, route === 'work' ? 'TaskDetail' : '')
    const render = () => app.render(route === 'work' ? 'TaskDetail' : 'default', { botId: 'bot', workId: 'work' })
    const emptyTitle = (elements: ReturnType<typeof app.render>) => elements.find(({ type }) => type === 'EmptyState')?.props.title
    try {
      assert.match(String(emptyTitle(render())), /正在读取/)
      assert.deepEqual(views, ['other'], 'read-only routes never acknowledge chat notifications')
      generation = 2
      shared = { ...shared, generation, isCurrent: () => generation === 2, selectBot: () => next.promise.then(() => {}) }
      render()
      next.reject(new Error('当前读取失败'))
      await settleComponent()
      assert.match(String(emptyTitle(render())), /当前读取失败/)
      old.resolve(undefined)
      await settleComponent()
      assert.match(String(emptyTitle(render())), /当前读取失败/, 'an old successful select cannot erase this connection error')
      const retry = render().find(({ type, props }) => type === 'Button' && typeof props.onPress === 'function')
      assert.ok(retry && typeof retry.props.onPress === 'function')
      shared.selectBot = async () => {}
      retry.props.onPress()
      render()
      await settleComponent()
      shared.conversationId = 'chat'
      let elements = render()
      assert.equal(elements.some(({ type }) => typeof type === 'function' && type.name === 'ContextRecords'), false, 'matching selection alone cannot expose a foreign payload')
      assert.doesNotMatch(componentText(elements), /秘密记录/)
      shared.data = parseConversation({ ...context, works: [parityWork()] })
      render()
      await settleComponent()
      elements = render()
      assert.match(componentText(elements), /当前连接尚未完成首读/)
      shared.synced = true
      shared.bots = []
      assert.match(String(emptyTitle(render())), /找不到这个 Bot/)
      shared.bots = [parityBot]
      shared.data = parseConversation(context)
      elements = render()
      if (route === 'work') assert.match(componentText(elements), /来源：共享历史中的工作记录/, 'a known work stays visible when Main no longer includes it')
      else assert.equal(elements.some(({ type }) => typeof type === 'function' && type.name === 'ContextRecords'), true)
      shared.connectionOnline = false
      elements = render()
      assert.match(componentText(elements) + String(emptyTitle(elements)), /状态未知/)
    } finally { app.close() }
  }
})

test('native context renders shared memory, external attribution, scheduler states and honest sent records', () => {
  const shared = parityShared()
  shared.data = parseConversation({ ...context,
    bot_settings: { skills: [], memory: [{ memory_id: 'm', text: '共享笔记', metadata: { epoch: 1, revision: 'a'.repeat(64), control_revision: 0, source_event_hash: 'b'.repeat(64), origin: 'agent', user_controlled: false, essential: true } }], memory_sync_conflicts: { items: [], has_more: false }, schedules: [] },
    scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:p', state: 'publishing' }, { scope: { kind: 'main' as const }, trigger: 'message:d', state: 'completed' }], unanswered: [] },
    sessions: [{ node: 'worker', agent: 'codex', session_id: 'external', cwd: '/shared', summary: '外部摘要', muted: true, created_at_ms: 1 }],
    sent: [{ message_id: 'sent', to: 'chat', text: '协作请求', created_at_ms: 2 }],
  })
  shared.nodes = [{ ...parityNode, online: false }]
  const app = componentHarness('../../../../../apps/mobile/app/b/[botId]/context.tsx', {
    'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) }, '@/data/live': { useLive: () => shared },
    '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    '@vibeface/client/workspace/bot-settings.ts': { formatWhen: () => '刚才', scheduleFrequency: () => '一次' },
    '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, SettingsGroup: 'SettingsGroup', SettingsRow: 'SettingsRow', Details: 'Details' },
  })
  try {
    const elements = app.render('default')
    const records = elements.find(({ type }) => typeof type === 'function' && type.name === 'ContextRecords')
    assert.ok(records && typeof records.type === 'function')
    records.type(records.props)
    assert.equal(elements.some(({ props }) => props.label === 'Bot 自身会话' && String(props.description).includes('未知')), true)
    assert.equal(elements.some(({ props }) => String(props.description).includes('已静音 · 设备不可达')), true)
    assert.equal(elements.some(({ props }) => props.label === '后台工作历史 · 1'), true)
    const job = elements.find(({ type, props }) => typeof type === 'function' && type.name === 'JobObservation' && (props.job as { state: string }).state === 'publishing')
    assert.ok(job && typeof job.type === 'function')
    assert.match(componentText(job.type(job.props)), /回复同步中/)
    const message = elements.find(({ type }) => typeof type === 'function' && type.name === 'MessageRecord')
    assert.ok(message && typeof message.type === 'function')
    assert.match(componentText(message.type(message.props)), /对方是否收到或回复尚不清楚/)
    assert.equal(elements.some(({ props }) => props.href === '/b/bot'), true)
  } finally { app.close() }
})

test('context on both platforms separates compilation, sending, native cache and committed memory', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const projection = {
      attempt_id: 'attempt', provider: 'codex', stage: 'compiled', session_generation: 4,
      scope: { kind: 'main' }, sources: [], sources_omitted: 0, inputs: [], inputs_omitted: 0, stable_fingerprint: 'a'.repeat(64), dynamic_fingerprint: 'b'.repeat(64), stable_bytes: 100, dynamic_bytes: 200,
      invalidation_reason: 'stable_prompt_or_provider_changed', session_status: { resumable: true, pending_generation: true },
      coverage: { messages_omitted: 1, works_omitted: 0, dispatches_omitted: 0, questions_omitted: 0, attachments_omitted: 0, memory_omitted: 3, checkpoint_omitted: 1, handoff_omitted: 0, pending_checks_omitted: 2, bot_indexes_omitted: { skills: 4 }, history_retrieval: 'context' },
      native_token: 'private-native-token', rendered: 'private-prompt',
    }
    shared.data = parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [], unanswered: [], context: projection }, bot_settings: {
      skills: [], schedules: [], memory_staged_count: 2, memory_sync_conflicts: { items: [], has_more: false },
      memory_conflicts: [{ history_id: 'conflict', call_ref: 'remember', memory_id: 'memory', expected_epoch: 1, actual_epoch: 1, expected_revision: 'old', actual_revision: 'new', reason: 'revision_changed' }],
      memory: [{ memory_id: 'memory', text: '已保存的事实', metadata: { epoch: 1, revision: 'a'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: true, source_event_hash: 'b'.repeat(64) } }],
    } })
    const dependencies = {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/bot-settings.ts': { scheduleFrequency: () => '一次', formatWhen: () => '刚才' },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }, '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, SettingsGroup: 'SettingsGroup', SettingsRow: 'SettingsRow', Details: 'Details' },
    }
    const app = componentHarness(platform === 'web' ? './context-panel.tsx' : '../../../../../apps/mobile/app/b/[botId]/context.tsx', dependencies)
    const render = () => nestedElements(app.render(platform === 'web' ? 'ContextPanel' : 'default', { bot: parityBot, data: shared.data, nodes: shared.nodes, connected: shared.connectionOnline, memory: shared, onBack() {} }), ['AgentContinuity', 'ContinuityLayer', 'ContextDetails', 'MemorySummary', 'ContinuityStat', 'ContextRecords', 'Fold'])
    try {
      let elements = render()
      const text = componentText(elements)
      assert.match(text, /已编译/)
      assert.match(text, /会话代次 4/)
      assert.match(text, /下一轮生效/)
      assert.match(JSON.stringify(elements), /下轮重建/)
      assert(!JSON.stringify(elements).includes('可尝试续接'), 'a pending configuration cannot promise resuming the old identity')
      assert.match(text, /已保存的事实/)
      assert.match(text, /由你维护，受保护/)
      assert.match(text, /记忆已被更新，未覆盖新值/)
      assert(!JSON.stringify(elements).includes('private-native-token'))
      assert(!JSON.stringify(elements).includes('private-prompt'))
      shared.data!.scheduler!.context!.stage = 'started'
      assert.match(componentText(render()), /已发送/)
      shared.data!.scheduler!.context!.session_status = { resumable: true, pending_generation: false }
      assert.match(JSON.stringify(render()), /可尝试续接/)
      shared.connectionOnline = false
      elements = render()
      assert.match(JSON.stringify(elements), /当前状态未知/)
      assert(!JSON.stringify(elements).includes('可尝试续接'), 'a disconnected client does not treat cached reachability as a current observation')
      shared.connectionOnline = true
      shared.data!.scheduler!.context!.session_status = null
      elements = render()
      assert.match(JSON.stringify(elements), /续接状态未知/)
      shared.data!.scheduler!.remote = { owner: 'owner', reachable: false, jobs: [], scope_jobs: [], memory_conflicts: [] }
      assert.match(JSON.stringify(render()), /当前状态未知/)
    } finally { app.close() }
  }
})

test('native Stack focus restores non-chat views while the chat route owns its activation', () => {
  const views: string[] = []
  const stack = { Screen: 'Screen', Protected: 'Protected' }
  const app = componentHarness('../../../../../apps/mobile/app/_layout.tsx', {
    'expo-router': { Stack: stack, usePathname: () => '/' }, 'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) }, 'expo-linking': { useLinkingURL: () => null },
    '@/data/live': { LiveWorkspace: 'LiveWorkspace', useLive: () => ({ setView: (view: string) => { views.push(view) } }), useWaiting: () => 0 },
    '@/data/context': { useWorkspace: () => ({ session: { phase: 'ready' } }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({}) },
    'uniwind': { useCSSVariable: () => ['white', 'black'] }, '@/lib/layout': { useWide: () => false }, 'react-native': { Platform: { OS: 'ios' }, View: 'View' },
  })
  try {
    const elements = app.render('default')
    const screens = elements.find(({ type }) => typeof type === 'function' && type.name === 'Screens')
    assert.ok(screens && typeof screens.type === 'function')
    screens.type(screens.props)
    const navigation = elements.find(({ type }) => type === stack)?.props.screenListeners
    assert.ok(typeof navigation === 'function')
    for (const name of ['b/[botId]/context', 'b/[botId]', 'b/[botId]/work/[workId]', 'tasks']) navigation({ route: { name } }).focus()
    assert.deepEqual(views, ['other', 'other', 'tasks'])
    assert.equal(elements.some(({ props }) => props.name === 'b/[botId]/context'), true)
  } finally { app.close() }
})

test("task filters keep the counts, device confirmations and empty state within the selected Bot", () => {
  const bots = [{ bot_id: "a", conversation_id: "chat-a", name: "Bot A" }, { bot_id: "b", conversation_id: "chat-b", name: "Bot B" }]
  const work = (conversation_id: string) => ({ kind: "work", work_id: conversation_id, check_id: conversation_id, conversation_id, target_node: "offline", title: conversation_id, detail: "", reason: "offline" })
  let inbox = [
    { key: "device", kind: "confirm", check: { kind: "device", works: [work("chat-a"), work("chat-b")] } },
    { key: "question-b", kind: "question", done: false, question: { conversation_id: "chat-b", bot: bots[1], about: {} } },
    { key: "process", kind: "confirm", check: { kind: "process", check_id: "process", detail: "still running" } },
  ]
  const workspace = { bots, selectedBot: bots[1], openWorks: [], nodes: [], questions: [], liveProgress: [], connectionOnline: true, synced: true, answered: {} }
  const app = componentHarness("./tasks-overview.tsx", {
    "@vibeface/client/workspace/context.tsx": { useWorkspace: () => workspace },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
    "@vibeface/client/workspace/model.ts": workspaceModel,
    "@vibeface/client/workspace/questions.ts": workspaceQuestions,
    "@/lib/utils": { cn: (...values: unknown[]) => values.filter(Boolean).join(" ") },
    "./inbox": { useInbox: () => inbox },
    "./workspace-shell": { useCreateBot: () => () => {} },
  })
  const named = (elements: ReturnType<typeof app.render>, name: string) => elements.find(({ type }) => typeof type === "function" && type.name === name)
  try {
    let elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceMasthead")?.props.attention, 2)
    const selectBot = named(elements, "TaskFilters")?.props.onBotFilter
    assert.ok(typeof selectBot === "function")
    selectBot("a")
    elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceMasthead")?.props.attention, 1)
    const checks = nestedElements(elements, ["AttentionActivity"]).filter(({ type }) => type === "ConfirmCard").map(({ props }) => props.check)
    assert.deepEqual(JSON.parse(JSON.stringify(checks)), [{ kind: "device", works: [work("chat-a")] }])
    assert.equal(elements.some(({ type }) => type === "QuestionCard"), false)
    assert.equal(elements.find(({ props }) => props.role === "status")?.props.children, "1 项任务")
    inbox = inbox.filter((item) => item.kind === "question")
    elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceMasthead")?.props.attention, 0)
    assert.equal(named(elements, "WorkspaceEmpty")?.props.title, "没有符合筛选的任务")
  } finally { app.close() }
})

test("a device confirmation stays visible when Bot filtering removes the first Work", () => {
  const bots = [{ bot_id: "a", conversation_id: "chat-a", name: "Bot A" }, { bot_id: "b", conversation_id: "chat-b", name: "Bot B" }]
  const openWork = (conversation_id: string) => ({ work_id: `work-${conversation_id}`, conversation_id, trigger: "message:one", target_node: "offline", cwd: "/project", title: conversation_id, prompt: "检查", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null, kind: "codex", thread_id: `thread-${conversation_id}` })
  const works = [openWork("chat-a"), openWork("chat-b")]
  const inbox = [{ key: "device", kind: "confirm", check: { kind: "device", check_id: "device", detail: "", target_node: "offline", reason: "offline", works: works.map((work) => ({ kind: "work", check_id: `work:${work.work_id}`, detail: "", work_id: work.work_id, conversation_id: work.conversation_id, target_node: work.target_node, title: work.title, reason: "offline" })) } }]
  const workspace = { bots, selectedBot: bots[1], openWorks: works, nodes: [{ device_id: "offline", name: "离线设备", local: false, online: false, revoked: false }], questions: [], liveProgress: [], connectionOnline: true, synced: true, answered: {} }
  const app = componentHarness("./tasks-overview.tsx", {
    "@vibeface/client/workspace/context.tsx": { useWorkspace: () => workspace },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
    "@vibeface/client/workspace/model.ts": workspaceModel,
    "@vibeface/client/workspace/questions.ts": workspaceQuestions,
    "@/lib/utils": { cn: (...values: unknown[]) => values.filter(Boolean).join(" ") },
    "./inbox": { useInbox: () => inbox },
    "./workspace-shell": { useCreateBot: () => () => {} },
  })
  try {
    let elements = app.render("TasksOverview")
    const selectBot = elements.find(({ type }) => typeof type === "function" && type.name === "TaskFilters")?.props.onBotFilter
    assert.ok(typeof selectBot === "function")
    selectBot("b")
    elements = app.render("TasksOverview")
    const checks = nestedElements(elements, ["TaskOverviewCard"]).filter(({ type }) => type === "ConfirmCard").map(({ props }) => props.check)
    assert.deepEqual(JSON.parse(JSON.stringify(checks)).map((check: { works: { conversation_id: string }[] }) => check.works.map((work) => work.conversation_id)), [["chat-b"]])
  } finally { app.close() }
})

test("mobile task attention counts unanswered questions and program checks and opens their inbox", () => {
  let inbox: unknown[] = [{ kind: "question", done: false }, { kind: "confirm", check: { kind: "process" } }, { kind: "question", done: true }]
  const app = componentHarness("../../../../../apps/mobile/src/features/home/tasks.tsx", {
    "@/data/live": { useLive: () => null, useInbox: () => inbox },
    "@/data/context": { useWorkspace: () => ({ bots: [] }) },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
  })
  try {
    let elements = app.render("TasksScreen")
    const attention = () => elements.find(({ type }) => typeof type === "function" && type.name === "MobileMasthead")?.props.attention
    const empty = () => elements.some(({ type }) => typeof type === "function" && type.name === "MobileEmpty")
    assert.equal(attention(), null, "counts remain unknown without the live connection first read")
    assert.equal(elements.some(({ type, props }) => type === "Link" && props.href === "/inbox"), true)
    assert.equal(empty(), false)
    inbox = []
    elements = app.render("TasksScreen")
    assert.equal(attention(), null)
    assert.equal(empty(), true)
    assert.equal(elements.find(({ type }) => typeof type === "function" && type.name === "MobileEmpty")?.props.read, false)
  } finally { app.close() }
})

test("mobile task overview shows an unbound question summary and Bot entry", () => {
  const shared = parityShared()
  shared.openWorks = []
  shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], questions: [{
    question_id: "overview-question", conversation_id: "chat", bot: parityBot, text: "需要确认交付方式", severity: "attention", choices: [],
    request_id: null, work_id: null, about: { node: "worker", agent: "codex", session_id: "session", cwd: "/project", summary: "" }, created_at_ms: 1,
  }] })
  const app = componentHarness("../../../../../apps/mobile/src/features/home/tasks.tsx", {
    "@/data/live": { useLive: () => shared, useInbox: () => [] },
    "@/data/context": { useWorkspace: () => ({ bots: shared.bots }) },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
    "@vibeface/client/workspace/model.ts": workspaceModel,
    "@/lib/utils": { cn: (...values: unknown[]) => values.filter(Boolean).join(" ") },
  })
  try {
    const elements = app.render("TasksScreen")
    assert.equal(elements.some(({ type }) => typeof type === "function" && type.name === "MobileQuestionAttention"), true)
    assert.equal(nestedElements(elements, ["MobileQuestionAttention"]).some(({ type, props }) => type === "Link" && props.href === "/b/bot"), true)
  } finally { app.close() }
})

test('bound permission choices show readable actions and return their original signed labels on both platforms', async () => {
  for (const platform of ['web', 'native']) for (const selected of ['allow_once', 'deny']) {
    const labels = { allow_once: '{"decision":"accept"}', deny: '{"decision":"decline"}' }
    const question = workspaceModel.parseQuestions({ questions: [{ question_id: 'permission', conversation_id: 'chat', target: { kind: 'work', work_id: 'work' }, bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '是否允许本次读取？', choices: [{ label: labels.allow_once, action: 'allow_once' }, { label: labels.deny, action: 'deny' }], about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'held-request' }, author: 'worker', created_at_ms: 1, answer: null, closed: null }] })[0]
    assert.ok(question)
    const sent: string[] = [], request = pendingResult()
    const shared = { ...parityShared(), pendingAnswers: [] as { question_id: string; text: string }[], answer: async (_question: unknown, value: string) => { sent.push(value); return request.promise } }
    const app = componentHarness(platform === 'web' ? './question-card.tsx' : '../../../../../apps/mobile/src/features/chat/question-card.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@/data/context': { useWorkspace: () => ({ members: [], bots: shared.bots, actions: { answer: shared.answer } }) },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'client' } } }) },
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/lib/errors.ts': errors,
      '@/lib/glossary': { terms: { approval: { icon: 'Hand' } } },
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, run: () => Promise<void>) => { try { await run() } catch {} } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      'expo-haptics': { notificationAsync: async () => {}, NotificationFeedbackType: { Success: 'success' } },
    })
    const render = () => app.render('QuestionCard', { question })
    const readable = selected === 'allow_once' ? '允许本次' : '拒绝'
    const raw = selected === 'allow_once' ? labels.allow_once : labels.deny
    const press = () => {
      const button = render().find(({ type, props }) => type === 'Button' && (props.accessibilityLabel === readable || componentText(props.children) === readable))
      assert.ok(button)
      if (platform === 'web') { assert.ok(button.props.onClick); button.props.onClick() }
      else { assert.ok(typeof button.props.onPress === 'function'); button.props.onPress() }
    }
    try {
      const elements = render()
      assert.match(componentText(elements), /允许本次/)
      assert.match(componentText(elements), /拒绝/)
      assert.doesNotMatch(componentText(elements), /decision|accept|decline/)
      assert.equal(elements.some(({ type }) => type === 'textarea' || type === 'TextInput'), false)
      assert.doesNotMatch(componentText(elements), /忽略/, 'a held approval takes only its own actions')
      shared.writable = false; press(); assert.equal(sent.length, 0)
      shared.writable = true; press(); assert.deepEqual(sent, [raw])
      shared.pendingAnswers = [{ question_id: question.question_id, text: raw }]
      request.reject(new Error('connection lost')); await settleComponent()
      assert.match(componentText(render()), new RegExp(readable))
      assert.doesNotMatch(componentText(render()), /decision|accept|decline/)
      shared.pendingAnswers = []
      shared.answered = { [question.question_id]: raw }
      assert.match(componentText(render()), selected === 'allow_once' ? /已回答 · 允许本次/ : /已拒绝/)
      assert.doesNotMatch(componentText(render()), /decision|accept|decline/)
    } finally { app.close() }
  }
})

test("question cards send a choice on tap, send written text from the last line, ignore on demand, and keep unknown retries and winning answers", async () => {
  const sent: unknown[][] = [], notices: string[] = []
  let actionBusy: string | null = null, answerStatus = 'answered'
  const workspace = { generation: 1, writable: true, nodes: [], answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], announce: (notice: string) => { notices.push(notice) },
    bots: [{ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot' }], openWorks: [], data: { works: [{ work_id: 'w', thread_id: 's', title: '升级依赖', outcome: 'completed' }] },
    answer: async (...args: unknown[]) => { sent.push(args); return { status: answerStatus, text: answerStatus === 'answered' ? args[1] : '停下' } } }
  const toasts: { text: string; action?: { label: string; onClick(): void } }[] = [], visited: unknown[] = []
  const kit = new Proxy({ useAction: () => ({ busy: actionBusy, error: null, run: (_key: string, work: () => Promise<void>) => work() }), toast: (text: string, options: { action?: { label: string; onClick(): void } } = {}) => { toasts.push({ text, action: options.action }) } }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const router = { Link: 'Link', useNavigate: () => (target: unknown) => { visited.push(target) } }
  const app = componentHarness('./question-card.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'web', source: 'membership', can_manage: false } } }) }, '@vibeface/client/workspace/labels.ts': { nodeName: () => '工作电脑' }, '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject }, '@/components/app/kit': kit, '@tanstack/react-router': router })
  const longChoice = '只在本机验证完整配置并保留现有服务，检查通过后再决定是否发布'
  const question = { target: { kind: 'bot_main', bot_id: 'bot' }, question_id: 'q1', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '\n是否允许继续？\n已完成本机检查。\n\n线上结果未知。', choices: [{ label: '允许' }, { label: longChoice }, { label: '停下' }], about: { node: 'worker', agent: 'codex', session_id: 's', cwd: '/path/to/web/', summary: '改登录' }, author: 'gateway', created_at_ms: 1, answer: null, closed: null }
  const render = (props: object = {}) => app.render('QuestionCard', { question, ...props })
  const tap = async (label: string, props: object = {}) => {
    const button = render(props).find(({ type, props }) => (type === 'button' || type === 'Button') && (props['aria-label'] === label || componentText(props.children).includes(label)))
    assert.ok(button?.props.onClick, `missing button ${label}`)
    button.props.onClick(); await settleComponent()
  }
  const write = (value: string) => {
    const input = render().find(({ type }) => type === 'textarea')
    assert.ok(input?.props.onChange)
    input.props.onChange({ target: { value } })
  }
  const plain = (value: unknown) => JSON.parse(JSON.stringify(value))
  try {
    let elements = render()
    assert.match(componentText(elements), /web[^]*工作电脑/, 'the context identifies its project and device')
    assert.deepEqual(elements.filter(({ type }) => type === 'button').map(({ props }) => componentText(props.children)), ['1允许', `2${longChoice}`, '3停下'], 'one numbered row per real choice')
    const input = elements.find(({ type }) => type === 'textarea')
    assert.equal(input?.props.rows, 1, 'the last line waits for a written answer')
    assert.equal(input?.props['aria-label'], '你的回答')
    assert.equal(input?.props.onKeyDown, undefined, 'Enter and IME stay native textarea input')
    assert.equal(elements.find(({ type, props }) => type === 'Button' && props['aria-label'] === '发送回答')?.props.disabled, true, 'an empty answer cannot be sent')
    assert.ok(elements.some(({ props }) => props.text === '已完成本机检查。\n\n线上结果未知。'), 'the decision context keeps its paragraphs')
    await tap('允许')
    assert.deepEqual(plain(sent.at(-1)), [question, '允许'], 'a tap sends the exact choice')
    await tap(longChoice)
    assert.deepEqual(plain(sent.at(-1)), [question, longChoice], 'the complete choice is submitted')
    for (const value of ['1', '１', '先检查配置\n确认后再继续']) {
      write(value)
      await tap('发送回答')
      assert.deepEqual(plain(sent.at(-1)), [question, value], 'custom text is literal, including digits and newlines')
    }
    const count = sent.length
    write('   ')
    assert.equal(render().find(({ type, props }) => type === 'Button' && props['aria-label'] === '发送回答')?.props.disabled, true, 'whitespace cannot be submitted')
    write('新回答')
    workspace.writable = false
    elements = render()
    assert.equal(elements.filter(({ type }) => type === 'button' || type === 'textarea').every(({ props }) => props.disabled === true), true)
    await tap('停下'); await tap('忽略')
    assert.equal(sent.length, count, 'the send handler also enforces read-only state')
    workspace.writable = true
    await tap('忽略')
    assert.deepEqual(plain(sent.at(-1)), [question, ignoredAnswer], 'ignoring sends the fixed answer')
    workspace.pendingAnswers = [{ question_id: 'q1', text: '继续\n原文' }]
    elements = render()
    assert.equal(elements.some(({ type }) => type === 'button' || type === 'textarea'), false, 'a pending answer cannot be replaced')
    assert.match(componentText(elements), /没确认送达/)
    const retry = elements.find(({ type, props }) => type === 'Button' && componentText(props.children).includes('再试'))
    assert.ok(retry?.props.onClick)
    retry.props.onClick(); await settleComponent()
    assert.deepEqual(plain(sent.at(-1)), [question, '继续\n原文'], 'unknown results retry their original text')
    actionBusy = 'retry'
    elements = render()
    assert.match(componentText(elements), /正在发送/)
    assert.doesNotMatch(componentText(elements), /没确认送达/)
    assert.equal(elements.find(({ type }) => type === 'Button')?.props.disabled, true)
    actionBusy = null
    workspace.answered = { q1: '允许' }
    elements = render({ question: { ...question, closed: 'request_lost', answer: { text: '停下' } }, answer: '过时答案' })
    assert.match(componentText(elements), /已由另一端回答：停下/, 'the network winner takes precedence over a folded answer, closed state and pending retry')
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'button'), false)
    workspace.pendingAnswers = []
    elements = render()
    assert.match(componentText(elements), /已回答 · 允许/, 'a shared answer folds the card immediately')
    assert.doesNotMatch(componentText(elements), /升级依赖/, 'an ordinary question names no held task')
    elements = render({ question: { ...question, about: { ...question.about, request_id: 'r' } } })
    assert.match(componentText(elements), /升级依赖[^]*工作电脑/)
    workspace.answered = { q1: ignoredAnswer }
    assert.match(componentText(render()), /已忽略/, 'an ignored question folds as ignored')
    workspace.answered = {}
    elements = render({ question: { ...question, closed: 'request_lost' } })
    assert.match(componentText(elements), /已失效/)
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'button'), false)
    await tap('允许', { inbox: true })
    await tap('忽略', { inbox: true })
    assert.deepEqual(toasts.map(({ text }) => text), ['已回答', '已忽略'])
    toasts[0].action?.onClick()
    assert.deepEqual(plain(visited), [{ to: '/b/$botId', params: { botId: 'bot' } }])
    answerStatus = 'already_answered'
    await tap('允许', { inbox: true })
    assert.deepEqual(notices, ['这个问题已经回答过了'], 'the existing race announcement is retained')
    assert.equal(toasts.length, 2, 'a losing answer has no success toast')
  } finally { app.close() }
})

test("a question without choices offers only the answer line and 忽略", () => {
  const app = componentHarness('./question-card.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ writable: true, nodes: [], answered: {}, pendingAnswers: [], bots: [], openWorks: [] }) },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备' },
    '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject },
    '@/components/app/kit': { useAction: () => ({ busy: null, error: null }) },
    '@tanstack/react-router': { useNavigate: () => () => {} },
  })
  try {
    const elements = app.render('QuestionCard', { question: { question_id: 'free', text: '需要什么配置？', choices: [], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: null, closed: null } })
    assert.equal(elements.find(({ type }) => type === 'textarea')?.props.rows, 1)
    assert.equal(elements.some(({ type }) => type === 'button'), false)
    assert.ok(elements.some(({ type, props }) => type === 'Button' && componentText(props.children).includes('忽略')))
  } finally { app.close() }
})

test("mobile questions send a choice on tap, preserve literal text, ignore on demand and retry only the unknown answer", async () => {
  for (const value of ['允许', '1', '１', '先核对配置\n确认后再继续', ignoredAnswer]) {
    const sent: unknown[][] = []
    let hold = pendingResult()
    const shared = { writable: true, answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], openWorks: [], nodes: [] }
    const question = { question_id: 'mobile', text: '\n是否允许继续？\n线上结果未知。', choices: [{ label: '允许' }, { label: '先核对配置' }, { label: '停下' }, { label: '保持现状' }], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: null, closed: null }
    const app = componentHarness('../../../../../apps/mobile/src/features/chat/question-card.tsx', {
      '@/data/context': { useWorkspace: () => ({ members: [], bots: [], actions: { answer: (...args: unknown[]) => { sent.push(args); return hold.promise } } }) },
      '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'mobile', source: 'membership', can_manage: false } } }) },
      '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject },
      '@vibeface/client/lib/errors.ts': errors,
      'expo-haptics': { notificationAsync: async () => {}, NotificationFeedbackType: { Success: 'success' } },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
    })
    const render = (props: object = {}) => app.render('QuestionCard', { question, ...props })
    const press = (label: string) => {
      const onPress = render().find(({ props }) => props.accessibilityLabel === label)?.props.onPress
      assert.ok(typeof onPress === 'function', `missing mobile control ${label}`)
      onPress()
    }
    const words = () => componentText(render())
    try {
      assert.deepEqual(render().filter(({ props }) => props.accessibilityRole === 'button' && props.onPress).map(({ props }) => props.accessibilityLabel), ['允许', '先核对配置', '停下', '保持现状'], 'one row per choice')
      const input = render().find(({ type }) => type === 'TextInput')
      assert.equal(input?.props.multiline, true)
      assert.equal(input?.props.submitBehavior, 'newline')
      assert.equal(input?.props.onSubmitEditing, undefined)
      const control = value === '允许' ? '允许' : value === ignoredAnswer ? '忽略' : '发送回答'
      if (control === '发送回答') {
        assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送回答')?.props.disabled, true, 'an empty answer cannot be sent')
        const write = input?.props.onChangeText
        assert.ok(typeof write === 'function')
        write(value)
        assert.equal(sent.length, 0, 'writing is local')
      }
      shared.writable = false
      press(control)
      assert.equal(sent.length, 0, 'read-only state also guards the handler')
      shared.writable = true
      press(control); press(control)
      assert.equal(sent.length, 1, 'the busy ref blocks a second tap before React rerenders')
      assert.equal(sent[0][1], value)
      shared.pendingAnswers = [{ question_id: 'mobile', text: value }]
      assert.match(words(), /正在发送/)
      assert.doesNotMatch(words(), /没确认送达/)
      hold.reject(new Error('connection lost')); await settleComponent()
      assert.match(words(), /没确认送达/)
      assert.equal(render().some(({ type, props }) => type === 'TextInput' || props.accessibilityLabel === '允许'), false)
      hold = pendingResult()
      const retry = render().find(({ type }) => type === 'Button')?.props.onPress
      assert.ok(typeof retry === 'function')
      retry(); retry()
      assert.equal(sent.length, 2)
      assert.equal(sent[1][1], value, 'retry preserves its original text')
      hold.resolve({ status: 'already_answered', text: '停下' }); await settleComponent()
      assert.match(words(), /已由另一端回答：停下/, 'the returned winner settles immediately without waiting for a snapshot')
      shared.answered = { mobile: '允许' }
      assert.ok(render({ question: { ...question, closed: 'request_lost', answer: { text: '停下' } } }).some(({ props }) => props.children === '已由另一端回答：停下'), 'standing answers precede closed and pending state')
    } finally { app.close() }
  }
})

test("remounted question cards identify the answer author and reserve warning styling for a lost race", () => {
  for (const platform of ['web', 'mobile']) {
    for (const scenario of [{ author: 'phone', mine: undefined, elsewhere: true, warned: false }, { author: 'gateway', mine: undefined, elsewhere: true, warned: false }, { author: 'self', mine: undefined, elsewhere: false, warned: false }, { author: 'phone', mine: '停下', elsewhere: true, warned: true }, { author: 'phone', mine: '继续', elsewhere: true, warned: false }]) {
      const question = { question_id: 'remount', text: '继续吗？\n线上结果未知。', choices: [{ label: '允许' }], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: { text: '继续', author: scenario.author }, closed: 'request_lost' }
      const shared = { writable: true, nodes: [], answered: scenario.mine === undefined ? {} : { remount: scenario.mine }, pendingAnswers: [{ question_id: 'remount', text: '旧草稿' }], bots: [], openWorks: [] }
      const app = componentHarness(platform === 'web' ? './question-card.tsx' : '../../../../../apps/mobile/src/features/chat/question-card.tsx', {
        '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
        '@/data/context': { useWorkspace: () => ({ members: [], bots: [] }) },
        '@/data/live': { useLive: () => shared },
        '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { principal: { device_id: 'self', source: 'membership', can_manage: false } } }) },
        '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备' },
        '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject },
        '@/components/app/kit': { useAction: () => ({ busy: null, error: null }) },
        '@tanstack/react-router': { useNavigate: () => () => {} },
      })
      try {
        const elements = app.render('QuestionCard', { question, answer: '过时的折叠答案' })
        const settled = elements.find(({ props }) => props.ask === '继续吗？')
        assert.ok(settled, `${platform} returns the standing answer before closed or pending state`)
        assert.equal(componentText(settled.props.children), scenario.elsewhere ? '已由另一端回答：继续' : '已回答 · 继续')
        const marker = settled.props.mark as { props: { className: string } } | undefined
        const className = platform === 'web' ? marker?.props.className : settled.props.className
        assert.equal(String(className).includes('text-warning'), scenario.warned, `${platform}: another author alone is not a failed race`)
        assert.equal(elements.some(({ type }) => type === 'Button' || type === 'input' || type === 'TextInput'), false)
      } finally { app.close() }
    }
  }
})

test("task overview keeps pending decisions with their task and every real task visible", () => {
  const work = {
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    resolved: { provider: 'codex', model: null }, trigger: 'message:one', target_node: 'offline', cwd: '/project', title: '检查项目', prompt: '', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null }
  const approvalRows = [{ target: { kind: 'work', work_id: 'held' }, question_id: 'approval', conversation_id: 'chat', bot: null, severity: 'attention', text: '继续吗？', choices: [{ label: '继续' }], about: { node: 'offline', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' }, author: 'offline', created_at_ms: 1, answer: null, closed: null }]
  const approvalQuestions = workspaceModel.parseQuestions({ questions: approvalRows })
  const data = parseConversation({ ...context, works: [{ ...work, work_id: 'held', thread_id: 'session' }, { ...work, work_id: 'running', target_node: 'online' }], questions: approvalRows })
  const works = data.works.map((item) => ({ ...item, conversation_id: 'chat' }))
  const nodes = parseNodes({ nodes: [{ device_id: 'offline', name: '离线电脑', online: false, local: false, revoked: false }, { device_id: 'online', name: '在线电脑', online: true, local: false, revoked: false }] })
  const overview = workspaceModel.overviewFromWorkspace({ openWorks: works, bots: [], questions: [], nodes })
  const workspace = { nodes, openWorks: works, bots: [], questions: approvalQuestions, overview, selectedBot: null, data, conversationId: 'chat', synced: true, connectionOnline: true, answered: {}, liveProgress: [] }
  const checks = workspaceModel.workChecks(works, nodes)
  const app = componentHarness('./tasks-overview.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/workspace/labels.ts': taskLabels,
    './inbox': { useInbox: () => [{ kind: 'question', done: false, question: approvalQuestions[0] }, ...checks.map((check) => ({ kind: 'confirm', check }))] },
    './workspace-shell': { useCreateBot: () => () => {} },
    '@tanstack/react-router': { Link: 'Link' },
  })
  try {
    const elements = app.render('TasksOverview')
    assert.deepEqual(elements.filter(({ props }) => props.work).map(({ props }) => (props.work as { work_id: string }).work_id), ['held', 'running'], 'a task waiting for confirmation remains visible before active tasks')
    assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === 'WorkspaceMasthead')?.props.attention, 1, 'the same task, question and confirmation are counted once')
    assert.equal(elements.filter(({ type }) => type === 'QuestionCard').length, 1)
    assert.equal(elements.filter(({ type }) => type === 'ConfirmCard').length, 1)
    assert.equal(elements.some(({ props }) => typeof props.children === 'string' && props.children === '2 项任务'), true)
  } finally { app.close() }
})

test("any entrance stops an unfinished task once, after a confirming second tap, even on another device", async () => {
  const stopped: string[] = []
  const data = { local_node: 'worker', works: [] as ReturnType<typeof parityWork>[], questions: [] }
  const workspace = { ...parityShared(), generation: 1, nodes: [], liveProgress: [], writable: true, pendingMessages: [], canSaveMessages: true, questions: [], data, interruptWork: async (work: string) => { stopped.push(work) }, async readWork(input: { conversation_id: string; work_id: string }) {
    const work = data.works.find((item) => item.work_id === input.work_id)
    if (!work) throw new Error('共享历史中找不到这个工作')
    return { work: { ...work, target: work.target ?? null, source_events: ['a'.repeat(64)], conversation_id: input.conversation_id }, coverage: { source: 'shared_history', complete: false } }
  } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { ...taskLabels, nodeName: () => '设备', promptSummary, taskActive: () => true, taskLabel: () => '', taskStale: () => false, staleTaskLabel: '', progressLabel: () => '', shownState: (work: { state: string }) => work.state, taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': workspaceQuestions }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  const render = async () => { workspace.data = { ...data }; workspace.openWorks = data.works.map((item) => ({ ...item, conversation_id: 'chat' })); app.render('TaskDetail', { botId: 'bot', workId: 'work' }); await settleComponent(); return app.render('TaskDetail', { botId: 'bot', workId: 'work' }) }
  const stop = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'destructive')
  const ask = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'outline' && [props.children].flat().some((child) => typeof child === 'string' && child.includes('停止')))
  try {
    const work = parityWork({ target_node: 'worker', cwd: '/p', title: '工作', prompt: '工作', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null })
    data.works = [{ ...work, state: 'unverified' }]
    assert.equal(stop(await render()), undefined)
    data.works = [{ ...work, target_node: 'other', stop: 'stop_waiting' }]
    assert.equal(stop(await render()), undefined, "a stop already on its way is not sent twice")
    data.works = [{ ...work, target_node: 'other' }]
    const first = await render()
    assert.equal(stop(first), undefined, 'one tap never stops a task')
    ;(ask(first)?.props.onClick as () => void)()
    ;(stop(await render())?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work'])
    const words = (elements: { props: Record<string, unknown> }[]) => elements.flatMap(({ props }) => [props.children].flat()).filter((child) => typeof child === 'string' || typeof child === 'number').join('')
    const turn = { thread_id: 'th', turn_id: 'tu' }
    data.works = [{ ...work, ...turn }, { ...work, ...turn, work_id: 'steered' }]
    const elements = await render()
    assert.match(words(elements), /会一起停止/, 'a shared turn lists what stops with it')
    assert.match(words(elements), /一起停止 2 个任务/)
    ;(ask(elements)?.props.onClick as () => void)()
    ;(stop(await render())?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work', 'work'], 'the count is named before the confirming tap stops them all')
  } finally { app.close() }
})

test("below lg the task panel is a modal dialog that makes the rest of the page inert, and beside the chat it is not", () => {
  const workspace = { ...parityShared(), generation: 1, nodes: [], liveProgress: [], writable: true, pendingMessages: [], canSaveMessages: true, questions: [], data: { local_node: 'worker', questions: [], works: [{ work_id: 'work', target_node: 'worker', cwd: '/p', title: '整理日志', prompt: '', state: 'completed', summary: '', outcome: 'completed', result_text: '好了', failure_reason: null }] } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  let wide = false
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { ...taskLabels, nodeName: () => '设备', promptSummary, taskActive: () => false, taskLabel: () => '', taskStale: () => false, staleTaskLabel: '', progressLabel: () => '', shownState: (work: { state: string }) => work.state, taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': workspaceQuestions }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: (query: string) => ({ matches: query === '(min-width: 1024px)' && wide }) } })
  // A page shaped like the shell: the panel's layer beside the chat, a header above, a sidebar and the toast region outside.
  type Node = { name: string; attributes: Set<string>; parentElement: Node | null; children: Node[]; hasAttribute(name: string): boolean; setAttribute(name: string): void; removeAttribute(name: string): void }
  const node = (name: string, children: Node[] = [], attributes: string[] = []): Node => {
    const self: Node = { name, attributes: new Set(attributes), parentElement: null, children, hasAttribute: (key) => self.attributes.has(key), setAttribute: (key) => { self.attributes.add(key) }, removeAttribute: (key) => { self.attributes.delete(key) } }
    for (const child of children) child.parentElement = self
    return self
  }
  const layer = node('layer'), chat = node('chat'), header = node('header'), sidebar = node('sidebar'), toasts = node('toasts', [], ['aria-live']), hidden = node('hidden', [], ['inert'])
  const head = node('head')
  node('html', [head, node('body', [node('shell', [sidebar, node('main', [header, node('row', [chat, layer])])]), toasts, hidden])])
  const inert = () => [chat, header, sidebar, toasts, hidden, layer, head].filter((item) => item.hasAttribute('inert')).map((item) => item.name)
  const render = () => {
    const elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    return { panel: elements.find(({ type }) => type === 'aside')?.props ?? {}, title: elements.find(({ type }) => type === 'h2')?.props ?? {}, layer: elements.find(({ props }) => props.className === 'contents')?.props.ref as { current: unknown } }
  }
  try {
    const first = render()
    assert.equal(first.panel.role, 'dialog')
    assert.equal(first.panel['aria-modal'], true)
    assert.equal(first.panel['aria-labelledby'], first.title.id, 'the dialog is named by its title')
    first.layer.current = layer
    wide = true
    const beside = render().panel
    assert.equal(beside.role, undefined, 'beside the chat the panel is not a dialog')
    assert.equal(beside['aria-modal'], undefined)
    assert.equal(beside['aria-label'], '任务详情')
    assert.deepEqual(inert(), ['hidden'], 'and nothing else is made inert')
    wide = false
    render()
    assert.deepEqual(inert(), ['chat', 'header', 'sidebar', 'hidden'], 'narrowing the window covers the page but not the panel, the toasts or <head>')
    wide = true
    render()
    assert.deepEqual(inert(), ['hidden'], 'widening it gives the page back, leaving what was inert before alone')
    wide = false
    render()
    app.close()
    assert.deepEqual(inert(), ['hidden'], 'closing the panel gives the page back')
  } finally { app.close() }
})

test("bot projects use online manager capabilities and preserve device names", () => {
  const node = { device_id: 'worker', name: '工作电脑', local: false, online: true, revoked: false, capabilities: { manager: 'codex', roots: ['/path/to/a/long/path/to/project'] } }
  const app = componentHarness('./bot-form.tsx', { '@tanstack/react-router': { useNavigate: () => () => {} }, '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ generation: 1, nodes: [node, { ...node, device_id: 'no-agent', name: '无 Agent', capabilities: { manager: null, roots: ['/unavailable'] } }], pendingBot: null, writable: true }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { nodeName }, '@vibeface/client/workspace/questions.ts': { projectName } })
  try {
    const projects = app.render('BotForm', { open: true, onOpenChange() {}, restoreFocus() {} }).filter(({ type, props }) => type === 'NativeSelectOption' && props.value)
    assert.equal(projects.length, 1)
    assert.equal(projects[0].props.children, '工作电脑 · project', 'the folder name stays readable in a narrow select')
  } finally { app.close() }
})

test("bot drafts survive a new connection while old create completion cannot close or unlock its form", async () => {
  let generation = 1, work = pendingResult()
  const closed: boolean[] = []
  const workspace = () => { const owner = generation; return { ...historyActions, generation: owner, isCurrent: () => owner === generation, nodes: [], pendingBot: null, writable: true, createBot: () => work.promise } }
  let currentWorkspace = workspace()
  const app = componentHarness('./bot-form.tsx', { '@tanstack/react-router': { useNavigate: () => () => {} }, '@vibeface/client/workspace/bot-settings.ts': { folderLabels, folderSetup, projectChoices, projectKey }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message } })
  const render = () => app.render('BotForm', { open: true, onOpenChange: (open: boolean) => closed.push(open), restoreFocus() {} })
  try {
    render(); let elements = render()
    elements.find(({ props }) => props.name === 'bot-name')?.props.onChange?.({ target: { value: 'keep my draft' } })
    const submit = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: { reportValidity: () => true } })
    submit(); assert.equal(render().find(({ props }) => props.type === 'submit')?.props.disabled, true)
    const old = work
    generation++; currentWorkspace = workspace(); render(); elements = render()
    assert.equal(elements.find(({ props }) => props.name === 'bot-name')?.props.value, 'keep my draft')
    assert.equal(elements.find(({ props }) => props.type === 'submit')?.props.disabled, false)
    work = pendingResult(); submit()
    old.reject(new Error('stale create error')); await settleComponent()
    assert.equal(render().find(({ props }) => props.type === 'submit')?.props.disabled, true)
    assert.equal(render().find(({ type }) => type === 'InlineError')?.props.error, '')
    assert.deepEqual(closed, [])
    work.resolve({}); await settleComponent()
    assert.deepEqual(closed, [false])
  } finally { app.close() }
})

test("conversation keeps unknown and newly edited drafts across connection changes", async () => {
  let generation = 1, work = pendingResult()
  const workspace = () => { const owner = generation; return {
    ...historyActions, generation: owner, isCurrent: () => owner === generation, conversationId: 'chat', fresh: new Set(), data: context, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: () => work.promise, run: async (action: () => Promise<void>) => { try { await action() } catch {} },
  } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const composer = () => app.render('ConversationScreen').find(({ type }) => type === 'PromptInput')?.props
  try {
    composer()?.onChange?.('keep original draft'); composer()?.onSubmit?.()
    work.reject(new Error('unknown')); await settleComponent()
    assert.equal(composer()?.value, 'keep original draft')
    work = pendingResult(); composer()?.onSubmit?.()
    generation++; currentWorkspace = workspace(); composer()
    work.resolve(true); await settleComponent()
    assert.equal(composer()?.value, 'keep original draft')
    work = pendingResult(); composer()?.onSubmit?.(); composer()?.onChange?.('newer unsent draft')
    work.resolve(true); await settleComponent()
    assert.equal(composer()?.value, 'newer unsent draft')
    work = pendingResult(); composer()?.onSubmit?.(); work.resolve(true); await settleComponent()
    assert.equal(composer()?.value, '')
  } finally { app.close() }
})

test("an old action completion cannot clear a new generation's busy state", async () => {
  let generation = 1
  const old = pendingResult(), fresh = pendingResult()
  const app = componentHarness('./present.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ generation }) } })
  const render = (action: () => Promise<unknown>) => app.render('ActionButton', { children: 'check', action }).find(({ type }) => type === 'Button')?.props
  try {
    render(() => old.promise)?.onClick?.()
    assert.equal(render(() => old.promise)?.disabled, true)
    generation++
    assert.equal(render(() => fresh.promise)?.disabled, false)
    render(() => fresh.promise)?.onClick?.()
    old.resolve(undefined); await settleComponent()
    assert.equal(render(() => fresh.promise)?.disabled, true)
    fresh.resolve(undefined); await settleComponent()
    assert.equal(render(() => fresh.promise)?.disabled, false)
  } finally { app.close() }
})

test("connection form keeps current errors and busy ownership when an earlier connection is cancelled", async () => {
  let generation = 1, work = pendingResult(), synchronousFailure = false
  const device = {
    connectionGeneration: () => generation, entries: [{ kind: 'gateway', address: 'https://node.example', device_id: 'target' }], initialInvitation: '',
    identity: { deviceId: 'browser' }, phase: 'disconnected', error: '', entryName: () => '',
    connect() { if (synchronousFailure) throw new Error('current invalid input'); generation++; return work.promise },
  }
  const app = componentHarness('../identity/connection-screen.tsx', { '@vibeface/client/identity/context.tsx': { useDevice: () => device, notJoined: (self: string) => `${self}还没加入`, removedText: (self: string) => `${self}已被移除` }, '@/lib/platform': { webPlatform: { self: '这个浏览器', transport: { connectDevice: async () => { throw new Error('offline') } } } }, '@vibeface/client/identity/connection-lane.ts': { primaryLane }, '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message, entryAddress, entryKey }, '@vibeface/client/workspace/labels.ts': { addressName: (address: string) => address, hostOf: () => '' } })
  const render = () => app.render('ConnectionScreen')
  const connect = () => render().find(({ type, props }) => type === 'Button' && props.title === 'https://node.example')?.props
  try {
    render(); render(); connect()?.onClick?.()
    assert.equal(connect()?.disabled, true)
    const old = work
    generation++; render(); assert.equal(connect()?.disabled, false)
    work = pendingResult(); connect()?.onClick?.()
    old.reject(new Error('stale connection error')); await settleComponent()
    assert.equal(connect()?.disabled, true)
    assert.equal(render().some(({ props }) => props.error), false)
    work.resolve(undefined); await settleComponent()
    assert.equal(connect()?.disabled, false)
    synchronousFailure = true; connect()?.onClick?.(); await settleComponent()
    assert.equal(render().find(({ props }) => props.error)?.props.error, 'current invalid input')
  } finally { app.close() }
})

test("artifact reads retain their offset while old responses cannot append or unlock newer reads", async () => {
  let generation = 1, work = pendingResult()
  const offsets: number[] = []
  const workspace = () => { const owner = generation; return { ...historyActions, generation: owner, isCurrent: () => owner === generation, connectionOnline: true,
    readArtifact: (_hash: string, offset: number) => { offsets.push(offset); return work.promise }, run: async (action: () => Promise<void>) => { try { await action() } catch {} } } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/elements/artifact.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/workspace/model.ts': { formatSize } })
  const render = () => app.render('Artifact', { file: { hash: 'result', size: 5, name: 'notes.txt' } })
  const open = () => render().find(({ props }) => props.label === '打开')?.props
  try {
    open()?.action?.(); assert.equal(open()?.disabled, true)
    const old = work
    generation++; currentWorkspace = workspace()
    assert.equal(open()?.disabled, false)
    work = pendingResult(); open()?.action?.()
    old.resolve({ size: 5, next_offset: null, text: 'stale' }); await settleComponent()
    assert.equal(open()?.disabled, true)
    work.resolve({ size: 5, next_offset: null, text: 'fresh' }); await settleComponent()
    assert.equal(render().find(({ type }) => type === 'pre')?.props.children, 'fresh')
    assert.deepEqual(offsets, [0, 0])
    assert.ok(render().some(({ type, props }) => type === 'span' && props.title === 'notes.txt' && (props.children as { props?: { name?: string } } | undefined)?.props?.name === 'notes.txt'), 'the card names the file')
  } finally { app.close() }
})

test("a long message of the user's folds behind an icon button; a short one does not", () => {
  const app = componentHarness('../chat/elements/message.tsx', {})
  const toggle = (text: string) => app.render('FoldedText', { text }).find(({ type }) => type === 'button')?.props
  assert.equal(toggle('短消息'), undefined)
  const lines = (count: number) => Array(count).fill('一行').join('\n')
  assert.equal(toggle(lines(12)), undefined)
  for (const text of [lines(13), 'x'.repeat(601)]) {
    assert.equal(toggle(text)?.['aria-expanded'], false)
    assert.ok(app.render('FoldedText', { text }).some(({ props }) => props.className === 'line-clamp-8'))
    toggle(text)?.onClick?.()
    assert.equal(toggle(text)?.['aria-expanded'], true)
    assert.equal(toggle(text)?.['aria-label'], '收起')
    assert.ok(!app.render('FoldedText', { text }).some(({ props }) => props.className === 'line-clamp-8'))
    toggle(text)?.onClick?.()
  }
})

test("a toast's action is a 44px touch target on phones and stays compact on desktop", () => {
  const app = componentHarness('../../components/app/kit.tsx', { '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') } })
  app.render('Toaster')
  const kit = app.module
  ;(kit.toast as (text: string, options: object) => void)('已发出', { persist: true, action: { label: '查看', onClick() {} } })
  const action = app.render('Toaster').find(({ props }) => props.children === '查看')?.props
  assert.match(String(action?.className), /(^| )h-11( |$)/)
  assert.match(String(action?.className), /md:h-7/)
})

test("a segmented control is one Tab stop whose arrow keys move and select, wrapping at the ends", () => {
  const app = componentHarness('../../components/app/kit.tsx', { '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') } })
  const options = ['light', 'dark', 'system'].map((value) => ({ value, label: value }))
  let value = 'dark', focused = -1
  const radios = options.map((_, index) => ({ focus: () => { focused = index } }))
  const radio = () => radios[options.findIndex((option) => option.value === value)] ?? radios[focused]
  const render = (props: object = {}) => app.render('Segmented', { label: '外观', value, options, onChange: (next: string) => { value = next }, ...props })
  const press = (key: string, props?: object) => {
    let prevented = false
    ;(render(props).find(({ props }) => props.role === 'radiogroup')?.props.onKeyDown as (event: unknown) => void)({ key, preventDefault: () => { prevented = true }, target: radio(), currentTarget: { querySelectorAll: () => radios } })
    return prevented
  }
  assert.deepEqual(render().filter(({ props }) => props.role === 'radio').map(({ props }) => props.tabIndex), [-1, 0, -1])
  assert.ok(press('ArrowRight')); assert.equal(value, 'system'); assert.equal(focused, 2)
  press('ArrowDown'); assert.equal(value, 'light', 'wraps past the last option'); assert.equal(focused, 0)
  press('ArrowLeft'); assert.equal(value, 'system', 'wraps past the first option')
  press('ArrowUp'); assert.equal(value, 'dark')
  press('Home'); assert.equal(value, 'light')
  press('End'); assert.equal(value, 'system'); assert.equal(focused, 2)
  assert.equal(press('Tab'), false, 'Tab leaves the group')
  assert.equal(press('ArrowLeft', { disabled: true }), false); assert.equal(value, 'system')
  value = 'unknown'; focused = 1
  press('ArrowRight'); assert.equal(value, 'system', "arrows move from the focused option even when a change did not take")
  value = 'unknown'
  assert.deepEqual(render().filter(({ props }) => props.role === 'radio').map(({ props }) => props.tabIndex), [0, -1, -1], "with nothing checked the first option takes the Tab stop")
})

test('recovery restore clears submitted passwords before waiting and keeps the chosen file and name', async () => {
  let generation = 1, work = pendingResult()
  class Input { value = 'test-only-value'; files: object[] = [{}]; checked = false }
  const fields = new Map(['bundle', 'password', 'old_password', 'confirmation', 'name', 'bootstrap', 'address', 'device_id', 'relay'].map(name => [name, new Input()]))
  const passwords = ['password', 'old_password', 'confirmation'].map(name => fields.get(name))
  const form = { elements: { namedItem: (name: string) => fields.get(name) }, querySelectorAll: () => passwords }
  const device = { connectionGeneration: () => generation, ready: null, review: null, entry: null,
    beginRecovery: () => { generation++; return work.promise }, replaceRecoveryFile: () => work.promise }
  const app = componentHarness('../identity/recovery-controls.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/lib/node.ts': { parseEntry: (value: unknown) => value, messageOf: (cause: Error) => cause.message },
    '@vibeface/client/lib/secure.ts': { passphrase: (value: string) => value }, '@/lib/device-name': { defaultBrowserName: () => 'browser' },
  }, { HTMLInputElement: Input, HTMLTextAreaElement: Input })
  const render = () => app.render('RecoveryControls', { only: 'restore' })
  const submit = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: form })
  try {
    render(); render(); submit()
    assert.ok(passwords.every(field => field?.value === ''))
    assert.equal(fields.get('name')?.value, 'test-only-value')
    assert.equal(fields.get('bundle')?.files.length, 1)
    const old = work
    generation++; render()
    const password = fields.get('password'); assert.ok(password); password.value = 'replacement test-only-value'
    old.reject(new Error('stale package failure')); await settleComponent()
    assert.equal(password.value, 'replacement test-only-value')
    assert.equal(render().some(({ props }) => props.error), false)
    work = pendingResult(); submit(); assert.equal(password.value, '')
    work.resolve(undefined); await settleComponent()
    assert.equal(render().find(({ type }) => type === 'fieldset')?.props.disabled, false)
    const bundle = fields.get('bundle'); assert.ok(bundle); bundle.files = []
    password.value = 'another test-only-value'
    submit(); await settleComponent()
    assert.equal(password.value, '')
    assert.equal(render().find(({ props }) => props.error)?.props.error, '选一个恢复文件。')
  } finally { app.close() }
})

test('changing the recovery password keeps what was typed when it fails, and clears it once the new file is made', async () => {
  let work = pendingResult()
  class Input { value = 'test-only-value'; files: object[] = [{}]; checked = false }
  const fields = new Map(['bundle', 'password', 'old_password'].map(name => [name, new Input()]))
  const passwords = ['password', 'old_password'].map(name => fields.get(name))
  const form = { elements: { namedItem: (name: string) => fields.get(name) }, querySelectorAll: () => passwords }
  const device = { connectionGeneration: () => 1, ready: null, review: null, entry: null, replaceRecoveryFile: () => work.promise }
  const app = componentHarness('../identity/recovery-controls.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/lib/node.ts': { parseEntry: (value: unknown) => value, messageOf: (cause: Error) => cause.message },
    '@vibeface/client/lib/secure.ts': { passphrase: (value: string) => value, passwordStrength: () => ({ level: 'strong' }) }, '@/lib/device-name': { defaultBrowserName: () => 'browser' },
  }, { HTMLInputElement: Input, HTMLTextAreaElement: Input })
  const render = () => app.render('RecoveryControls', { only: 'passphrase' })
  const form_ = () => render().find(({ type }) => type === 'form')?.props
  const submit = () => form_()?.onSubmit?.({ preventDefault() {}, currentTarget: form })
  try {
    render(); submit()
    work.reject(new Error('旧密码不对')); await settleComponent()
    assert.ok(passwords.every(field => field?.value === 'test-only-value'), 'a failed try keeps both passwords')
    assert.equal(render().find(({ props }) => props.error)?.props.error, '旧密码不对')
    form_()?.onChange?.({})
    assert.equal(render().some(({ props }) => props.error), false, 'a change takes the old error away')
    work = pendingResult(); submit(); work.resolve(undefined); await settleComponent()
    assert.ok(passwords.every(field => field?.value === ''), 'a new file clears both passwords')
  } finally { app.close() }
})

test('native chat folds runtime diagnostics and retains result checks without provider availability cards', async () => {
  let refreshed = 0
  const runtime = workspaceModel.parseTurnRuntime({ provider: 'codex', model: 'submitted-model', reasoning_effort: 'high', generation: 'generation', session_generation: 1, delivery_id: 'delivery', stage: 'record', execution: 'started', attempted_at_ms: 1, switched_from: null, switch_reason: null })
  const job: ManagerJob = { scope: { kind: 'main' }, trigger: 'message:one', message_id: 'one', reply_message_id: 'reply', state: 'completed', runtime }
  const shared = { ...parityShared(), refresh: async () => { refreshed++ }, nodes: [{ ...parityNode, capabilities: { ...parityNode.capabilities, manager_state: 'unverified' as const } }] }
  shared.data = parseConversation({ ...context, messages: [{ message_id: 'reply', event_hash: 'reply', role: 'assistant', text: '完成', created_at_ms: 2 }], scheduler: { scope_jobs: [], jobs: [job], unanswered: [] } })
  const extras = componentHarness('../../../../../apps/mobile/src/features/chat/work.tsx', {
    '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/chat/protocol.ts': { promptIntent, canRetryJob, runtimeLabel }, '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleTrigger }, '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/workspace/questions.ts': { questionOpen }, '@vibeface/client/lib/errors.ts': errors,
  })
  const details = componentHarness('../../../../../apps/mobile/src/components/kit.tsx', {
    'react-native-reanimated': { default: { View: 'AnimatedView' }, useAnimatedStyle: (read: () => unknown) => read(), useSharedValue: (value: number) => ({ value }), withTiming: (value: number) => value }, 'uniwind': { withUniwind: (value: unknown) => value },
    '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
  })
  const render = () => { extras.render('useChatExtras', 'chat'); return extras.output as { after: Map<string, { key: string; node: HarnessElement }[]>; cards: HarnessElement[] } }
  try {
    const result = render()
    assert.equal(botReach(shared.nodes), 'unverified')
    assert.equal(result.cards.length, 0, 'unverified readiness is sendable without a provider card')
    const row = result.after.get('reply')?.find((row) => row.key === 'runtime:message:one')?.node
    assert.ok(row)
    assert.equal(row.type, 'Details')
    assert.equal(row.props.label, '运行信息')
    details.render('Details', row.props)
    assert.doesNotMatch(componentText(details.output), /Codex|submitted-model|提交推理强度|记录结果/)
    const toggle = details.render('Details', row.props).find(({ props }) => props.accessibilityRole === 'button')
    assert.deepEqual(JSON.parse(JSON.stringify(toggle?.props.accessibilityState)), { expanded: false })
    assert.equal(typeof toggle?.props.onPress, 'function')
    if (typeof toggle?.props.onPress === 'function') toggle.props.onPress()
    details.render('Details', row.props)
    assert.match(componentText(details.output), /Codex · 提交模型 submitted-model · 提交推理强度 high · 记录结果/)
    for (const current of [{ ...job, state: 'reconciling' }, { ...job, state: 'failed', pending_check: true }]) {
      shared.data.scheduler!.jobs = [current]
      const cards = render().cards
      assert.equal(cards.length, 1)
      assert.equal(cards[0].props.title, '这次回复的结果待确认')
      const action = findActionProps(cards[0].props.actions, '再查一次')?.action
      assert.equal(typeof action, 'function')
      if (typeof action === 'function') await action()
    }
    assert.equal(refreshed, 2, 'result reconciliation remains a read-only action')
    shared.data.works = [parityWork({ state: 'unverified', outcome: 'unverified', failure_reason: '无法证明结果' })]
    shared.data.checks = [{ check_id: 'process', conversation_id: 'chat', trigger: 'message:one', attempt_id: 'attempt', detail: '进程还在运行', process_groups: [42], created_at_ms: 1 }]
    const cards = render().cards
    assert.equal(cards.filter(({ type }) => typeof type === 'function' && type.name === 'ConfirmCard').length, 2, 'unknown work and running-process checks stay visible')
  } finally { extras.close(); details.close() }
})

function nativeChatHarness(os: string, dependencies: Record<string, unknown>) {
  return componentHarness('../../../../../apps/mobile/app/b/[botId].tsx', {
    'expo-router/react-navigation': { useHeaderHeight: () => 96 },
    'react-native': { Platform: { OS: os }, FlatList: 'FlatList', View: 'View', StyleSheet: { absoluteFill: {} } },
    'react-native-keyboard-controller': { useReanimatedKeyboardAnimation: () => ({ height: { value: 0 }, progress: { value: 0 } }) },
    'react-native-reanimated': { default: { View: 'AnimatedView' }, useAnimatedStyle: (read: () => unknown) => read(), useSharedValue: (value: number) => ({ value }) },
    'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 34 }), useSafeAreaFrame: () => ({ width: 390 }) },
    'uniwind': { withUniwind: (value: unknown) => value, useUniwind: () => ({ theme: 'light' }), useCSSVariable: () => 'white' },
    '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice: () => false, botReach, botCanSend, composerHint, inputTargetKey, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { messageTimes: () => new Map() },
    '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/home.ts': { unreachable }, '@vibeface/client/workspace/bot-settings.ts': { botCopy },
    '@/components/kit': { botTint: () => ({}), EmptyState: 'EmptyState' }, '@/components/bot-face': { BotFace: 'BotFace', ScreenFaces: 'ScreenFaces', botMood: () => 'idle' },
    '@/features/chat/work': { useChatExtras: () => ({ first: [], after: new Map(), rest: [], cards: [] }) }, '@/lib/layout': { useWide: () => false },
    ...dependencies,
  })
}

test('native composer adds photos and files without replacing current attachments or drafts', async (t) => {
  type Attachment = import('../../../../../apps/mobile/src/data/types.ts').Attachment
  type NativeButton = { text: string; onPress?: () => void; style?: string }
  for (const os of ['ios', 'android']) await t.test(os, async () => {
    const original = { uri: 'file:///report.pdf', name: 'report.pdf', size: 20, mimeType: 'application/pdf' }
    const current = { uri: 'file:///current.txt', name: 'current.txt', size: null, mimeType: null }
    const cache = new Map([
      ['file:///cache/original.heic', { name: 'original.heic', size: 30, type: 'image/heic' }],
      ['file:///cache/transparent.png', { name: 'transparent.png', size: 40, type: 'image/png' }],
      ['file:///cache/photo.jpg', { name: 'photo.jpg', size: 50, type: 'image/jpeg' }],
    ])
    let files: Attachment[] = [original], updates = 0, renders = 0, usedCallback = 0
    let photos = pendingResult(), documents = pendingResult(), selected: (() => void) | undefined
    const photoCalls: unknown[] = [], fileCalls: unknown[] = []
    const alerts: { title: string; message?: string; buttons?: NativeButton[] }[] = []
    let sheet: { options: string[]; cancelButtonIndex: number } | undefined, sheetSelect: ((index: number) => void) | undefined
    const app = componentHarness('../../../../../apps/mobile/src/features/chat/composer.tsx', {
      'react-native': {
        Platform: { OS: os }, TextInput: 'TextInput', View: 'View',
        ActionSheetIOS: { showActionSheetWithOptions(options: typeof sheet, select: typeof sheetSelect) { sheet = options; sheetSelect = select } },
        Alert: { alert(title: string, message?: string, buttons?: NativeButton[]) { alerts.push({ title, message, buttons }) } },
      },
      'expo-image-picker': {
        UIImagePickerPreferredAssetRepresentationMode: { Current: 'current' },
        launchImageLibraryAsync(options: unknown) { photoCalls.push(options); return photos.promise },
      },
      'expo-document-picker': { getDocumentAsync(options: unknown) { fileCalls.push(options); return documents.promise } },
      'expo-file-system': { File: class {
        readonly name: string; readonly size: number; readonly type: string
        constructor(uri: string) {
          const file = cache.get(uri)
          assert.ok(file)
          this.name = file.name; this.size = file.size; this.type = file.type
        }
      } },
    })
    const render = () => {
      const callback = ++renders
      return app.render('Composer', {
        chrome: { kind: 'solid' }, value: '保留草稿', onChange() {}, files, attachmentScope: JSON.stringify(['network', 'chat', ['bot_main', 'bot'], 'continue']),
        onFiles(next: Attachment[]) { usedCallback = callback; updates++; files = next }, blocked: null, sending: false, onSend() {},
      })
    }
    const choose = (index: number) => {
      const attach = render().find(({ props }) => String(props.accessibilityLabel).startsWith('添加附件'))?.props.onPress
      assert.equal(typeof attach, 'function')
      if (typeof attach === 'function') attach()
      if (os === 'ios') {
        assert.deepEqual(JSON.parse(JSON.stringify(sheet)), { options: ['照片图库', '文件', '取消'], cancelButtonIndex: 2 })
        selected = () => sheetSelect?.(index)
      } else {
        const menu = alerts.at(-1)
        assert.ok(menu)
        assert.equal(menu?.title, '添加附件')
        assert.deepEqual(Array.from(menu.buttons ?? [], ({ text }) => text), ['照片图库', '文件', '取消'])
        selected = menu.buttons?.[index]?.onPress
      }
      selected?.()
    }
    const snapshot = () => JSON.parse(JSON.stringify(files))
    try {
      choose(0)
      selected?.()
      assert.equal(photoCalls.length, 1, 'only one picker can run at once')
      assert.equal(fileCalls.length, 0)
      assert.deepEqual(JSON.parse(JSON.stringify(photoCalls[0])), { mediaTypes: ['images'], allowsMultipleSelection: true, quality: 1, preferredAssetRepresentationMode: 'current' })
      files = [current]; render()
      photos.resolve({ canceled: false, assets: [
        { uri: 'file:///cache/original.heic', fileName: null },
        { uri: 'file:///cache/transparent.png' },
        { uri: 'file:///cache/photo.jpg', fileName: 'holiday.jpg', fileSize: 50, mimeType: 'image/jpeg' },
      ] })
      await settleComponent()
      assert.equal(usedCallback, renders, 'completion uses the current draft setter')
      assert.deepEqual(snapshot(), [current,
        { uri: 'file:///cache/original.heic', name: 'original.heic', size: 30, mimeType: 'image/heic' },
        { uri: 'file:///cache/transparent.png', name: 'transparent.png', size: 40, mimeType: 'image/png' },
        { uri: 'file:///cache/photo.jpg', name: 'holiday.jpg', size: 50, mimeType: 'image/jpeg' },
      ])
      assert.equal(render().find(({ type }) => type === 'TextInput')?.props.value, '保留草稿')
      const selectedFiles = snapshot()
      photos = pendingResult(); choose(0); photos.resolve({ canceled: true, assets: null }); await settleComponent()
      choose(2)
      assert.equal(updates, 1, 'picker cancellation and menu cancellation leave the draft alone')
      assert.deepEqual(snapshot(), selectedFiles)
      photos = pendingResult(); choose(0); photos.reject(new Error('无法读取所选照片')); await settleComponent()
      assert.equal(alerts.at(-1)?.title, '未能添加附件')
      assert.equal(alerts.at(-1)?.message, '无法读取所选照片')
      assert.deepEqual(snapshot(), selectedFiles)
      photos = pendingResult(); choose(0); photos.reject(new Error('照片权限已拒绝')); await settleComponent()
      assert.equal(alerts.at(-1)?.message, '照片权限已拒绝')
      assert.deepEqual(snapshot(), selectedFiles, 'permission denial preserves the draft attachments')
      choose(1)
      assert.equal(fileCalls.length, 1, 'an error releases the picker for another attempt')
      assert.deepEqual(JSON.parse(JSON.stringify(fileCalls[0])), { multiple: true, copyToCacheDirectory: true })
      documents.resolve({ canceled: false, assets: [
        { uri: 'file:///report.pdf', name: 'report.pdf', size: 20, mimeType: 'application/pdf' },
        { uri: 'file:///notes.txt', name: 'notes.txt' },
      ] })
      await settleComponent()
      assert.deepEqual(snapshot(), [...selectedFiles, original, { uri: 'file:///notes.txt', name: 'notes.txt', size: null, mimeType: null }])
      documents = pendingResult(); choose(1); documents.resolve({ canceled: true, assets: null }); await settleComponent()
      assert.equal(updates, 2)
      const remove = render().find(({ props }) => props.accessibilityLabel === '移除 transparent.png')?.props.onPress
      assert.equal(typeof remove, 'function')
      if (typeof remove === 'function') remove()
      assert.equal(files.some(({ name }) => name === 'transparent.png'), false)
      assert.equal(files.some(({ name }) => name === 'holiday.jpg'), true)
    } finally { app.close() }
  })
})

test('native attachment results cannot cross a draft scope or survive its disabled or unmounted lifetime', async (t) => {
  type NativeButton = { text: string; onPress?: () => void }
  for (const os of ['ios', 'android']) for (const source of ['photos', 'files']) for (const change of ['bot', 'bot-roundtrip', 'network', 'blocked', 'blocked-roundtrip', 'sending', 'unmounted', 'stale-menu', 'blocked-before-menu', 'sending-before-menu']) await t.test(`${os} ${source} ${change}`, async () => {
    const live = parityShared()
    const conversation: import('../../../../../apps/mobile/src/data/types.ts').Conversation = {
      messages: [], before: new Map(), rest: [], answers: new Map(), hidden: new Set(), unread: null,
      works: [parityWork()], replying: false, status: null, stoppable: false, blocked: null,
    }
    const workspace = { bots: [parityBot, { ...parityBot, bot_id: 'other', conversation_id: 'other-chat' }], conversations: { chat: conversation, 'other-chat': { ...conversation } }, members: [], primary: null, actions: { open() {} }, connection: { entry: '工作设备' } }
    const device = { link: { kind: 'online' }, ready: { entry: { device_id: 'gateway' }, principal: { network_id: 'network', can_manage: true } } }
    let botId = 'bot', sending = false, calls = 0, selected: (() => void) | undefined
    const result = pendingResult()
    const screen = nativeChatHarness(os, {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId }) },
      '@/data/context': { useWorkspace: () => workspace }, '@/data/live': { useLive: () => live }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    })
    const composer = componentHarness('../../../../../apps/mobile/src/features/chat/composer.tsx', {
      'react-native': {
        Platform: { OS: os }, TextInput: 'TextInput', View: 'View',
        ActionSheetIOS: { showActionSheetWithOptions(_options: unknown, select: (index: number) => void) { selected = () => select(source === 'photos' ? 0 : 1) } },
        Alert: { alert(_title: string, _message?: string, buttons?: NativeButton[]) { selected = buttons?.[source === 'photos' ? 0 : 1]?.onPress } },
      },
      'expo-image-picker': { UIImagePickerPreferredAssetRepresentationMode: { Current: 'current' }, launchImageLibraryAsync() { calls++; return result.promise } },
      'expo-document-picker': { getDocumentAsync() { calls++; return result.promise } },
      'expo-file-system': { File: class { name = 'photo.jpg'; size = 20; type = 'image/jpeg' } },
    })
    const props = () => {
      screen.render('default')
      const current = screen.render('default').find(({ type }) => type === 'Composer')?.props
      assert.ok(current)
      return current
    }
    const render = () => composer.render('Composer', { ...props(), sending })
    try {
      props().onChange?.('A 草稿')
      const original = [{ uri: 'file:///a.txt', name: 'a.txt', size: 1, mimeType: 'text/plain' }]
      const onFiles = props().onFiles; assert.ok(typeof onFiles === 'function'); onFiles(original)
      const scope = props().attachmentScope
      assert.equal(typeof scope, 'string')
      if (change === 'blocked-before-menu') conversation.blocked = '当前设备仅可查看'
      if (change === 'sending-before-menu') sending = true
      const attach = render().find(({ props }) => String(props.accessibilityLabel).startsWith('添加附件'))?.props.onPress
      assert.ok(typeof attach === 'function'); attach()
      if (change.endsWith('-before-menu')) assert.equal(calls, 0, 'a disabled composer cannot open a picker even when its handler is invoked')
      else if (change !== 'stale-menu') { assert.ok(selected); selected(); assert.equal(calls, 1) }
      if (change === 'bot' || change === 'bot-roundtrip' || change === 'stale-menu') {
        botId = 'other'; render()
        props().onChange?.('B 草稿')
        const setFiles = props().onFiles; assert.ok(typeof setFiles === 'function')
        setFiles([{ uri: 'file:///b.txt', name: 'b.txt', size: 2, mimeType: 'text/plain' }]); render()
        if (change === 'bot-roundtrip') { botId = 'bot'; render(); assert.equal(props().attachmentScope, scope) }
      }
      if (change === 'network') { device.ready.principal.network_id = 'other-network'; render() }
      if (change === 'blocked' || change === 'blocked-roundtrip') { conversation.blocked = '当前设备仅可查看'; render(); if (change === 'blocked-roundtrip') { conversation.blocked = null; render() } }
      if (change === 'sending') { sending = true; render() }
      if (['bot', 'network', 'stale-menu'].includes(change)) assert.notEqual(props().attachmentScope, scope)
      if (change === 'unmounted') composer.close()
      if (change === 'stale-menu') { assert.ok(selected); selected(); assert.equal(calls, 0, 'a menu opened in A cannot start a picker in B') }
      if (change === 'blocked' || change === 'sending') {
        const button = render().find(({ props }) => String(props.accessibilityLabel).startsWith('添加附件'))?.props
        assert.equal(button?.disabled, true)
        assert.ok(typeof button?.onPress === 'function'); button.onPress(); assert.equal(calls, 1)
      }
      const before = JSON.stringify(props().files), text = props().value
      result.resolve({ canceled: false, assets: [{ uri: 'file:///photo.jpg', name: 'photo.jpg', fileName: 'photo.jpg', size: 20, mimeType: 'image/jpeg' }] })
      await settleComponent()
      assert.equal(JSON.stringify(props().files), before, 'late completion leaves the actual bound draft setter untouched')
      assert.equal(props().value, text)
      botId = 'bot'
      assert.equal(props().value, 'A 草稿')
      assert.equal(JSON.stringify(props().files), JSON.stringify(original), 'the original draft also retains its attachments')
    } finally { composer.close(); screen.close() }
  })
})

test('native composer preserves local input and visible blockers with compact chat chrome on iOS and Android', () => {
  for (const os of ['ios', 'android']) {
    let retried = 0
    const live = { ...parityShared(), nodes: parseNodes({ nodes: [{ ...parityNode, capabilities: { ...parityNode.capabilities, manager_state: 'unverified' } }] }) }
    const conversation: import('../../../../../apps/mobile/src/data/types.ts').Conversation = {
      messages: [{ message_id: 'one', event_hash: 'one', role: 'user', text: 'hello', created_at_ms: 1 }], before: new Map(), rest: [], answers: new Map(), hidden: new Set(), unread: null,
      works: [], replying: false, status: null, stoppable: false, blocked: null,
    }
    const workspace = { bots: [parityBot], conversations: { chat: conversation }, members: [], primary: null, actions: { open() {} }, connection: { entry: '工作设备' } }
    const device = { link: { kind: 'online' }, ready: { entry: { device_id: 'gateway' }, principal: { can_manage: true } }, retryNow: () => { retried++ } }
    const screen = nativeChatHarness(os, {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) },
      '@/data/context': { useWorkspace: () => workspace }, '@/data/live': { useLive: () => live }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
      '@/features/chat/chrome': { useChatChrome: () => ({ kind: 'solid' }) },
    })
    const composer = componentHarness('../../../../../apps/mobile/src/features/chat/composer.tsx', { 'react-native': { Platform: { OS: os }, TextInput: 'TextInput', View: 'View' } })
    const offline = componentHarness('../../../../../apps/mobile/src/components/connection-pill.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@/data/context': { useWorkspace: () => workspace },
      '@vibeface/client/identity/copy.ts': { offlineCopy }, '@vibeface/client/workspace/home.ts': { unreachable }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    })
    const props = () => screen.render('default').find(({ type }) => type === 'Composer')!.props
    const render = () => composer.render('Composer', { ...props(), value: 'draft' })
    try {
      assert.equal(props().blocked, null)
      assert.equal(componentText(props().children), '', 'unverified readiness adds no availability prompt')
      assert.equal(props().sending, false, 'network activity does not lock local saving')
      assert.equal(screen.render('default').filter(({ type }) => type === 'OfflineBar').length, 0)
      assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, false)
      device.link.kind = 'offline'
      assert.equal(screen.render('default').filter(({ type }) => type === 'OfflineBar').length, 1)
      assert.equal(props().blocked, null, 'a disconnected transport still permits durable local input')
      assert.equal(render().find(({ type }) => type === 'TextInput')?.props.placeholder, '发消息')
      const recovery = offline.render('OfflineBar', { chat: true })
      assert.equal(recovery.filter(({ props }) => props.role === 'status').length, 1)
      const retry = recovery.find(({ type }) => type === 'Button')?.props.onPress
      assert.equal(typeof retry, 'function')
      if (typeof retry === 'function') retry()
      assert.equal(retried, 1)
      assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, false)
      device.link.kind = 'online'
      for (const state of ['needs_setup', 'none', 'offline'] as const) {
        live.nodes = state === 'none' ? [] : parseNodes({ nodes: [{ ...parityNode, online: state !== 'offline', capabilities: { ...parityNode.capabilities, manager_state: 'needs_setup' } }] })
        const executionHint = composerHint({ hasConversation: true, loaded: true, bot: botReach(live.nodes) })
        assert.ok(executionHint)
        const elements = render()
        assert.equal(props().blocked, null, 'execution readiness does not block local saving')
        assert.equal(elements.find(({ type }) => type === 'TextInput')?.props.placeholder, '发消息')
        assert.equal(elements.find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, false)
        assert.equal(componentText(composer.output).split(executionHint).length - 1, 1)
        const links = screen.render('default').filter(({ type, props }) => type === 'Link' && componentText(props.children).includes(state === 'offline' ? '查看设备' : '去设置'))
        assert.equal(links.filter(({ props }) => props.href === (state === 'none' ? '/ai' : '/device/worker')).length, 1)
      }
      conversation.blocked = '当前设备仅可查看'
      assert.equal(render().find(({ type }) => type === 'TextInput')?.props.placeholder, conversation.blocked)
      assert.equal(componentText(composer.output).split(conversation.blocked).length - 1, 1, 'the write blocker remains visible beside any actionable children')
      assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, true)
    } finally { screen.close(); composer.close(); offline.close() }
  }
})

test("conversation preserves local input while execution devices are unavailable", () => {
  let sent = 0, refreshed = 0
  const workspace = (nodes: unknown[]) => ({
    ...historyActions, generation: 1, isCurrent: () => true, refresh: async () => { refreshed++ }, conversationId: 'chat', fresh: new Set(), data: context, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes, liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: async () => { sent++; return true }, run: async (action: () => Promise<void>) => { await action() },
  })
  let currentWorkspace = workspace([{ device_id: 'gateway', online: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }])
  const app = componentHarness('../chat/conversation-screen.tsx', { '@vibeface/client/workspace/labels.ts': taskLabels, '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const composer = () => app.render('ConversationScreen').find(({ type }) => type === 'PromptInput')?.props
  try {
    composer()?.onChange?.('hello'); composer()?.onSubmit?.()
    assert.equal(composer()?.disabled, false)
    assert.match(String(composer()?.reason), /运行 Bot/)
    assert.equal(sent, 1, 'offline execution does not block local input acceptance')
    assert.ok(refreshed > 0, "a bot no device can reply for reads readiness again")
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'needs_setup', roots: [] } }])
    assert.equal(composer()?.disabled, false, "execution setup does not lock the local composer")
    assert.equal(composer()?.hint, '', "an empty chat offers setup in its middle, not again under the box")
    assert.equal(composer()?.reason, '运行 Bot 的设备需设置', "the composer still explains the execution hold")
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }])
    assert.equal(composer()?.disabled, false)
    composer()?.onChange?.('second input')
    composer()?.onSubmit?.()
    assert.equal(sent, 2)
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'unverified', roots: [] } }])
    assert.equal(composer()?.disabled, false, 'unknown authentication can be tried without claiming readiness')
    assert.equal(composer()?.reason, '')
    assert.doesNotMatch(componentText(composer()?.children), /尚未确认|可用性|Codex/)
    composer()?.onChange?.('third input')
    composer()?.onSubmit?.()
    assert.equal(sent, 3)
    currentWorkspace.canSaveMessages = false
    assert.equal(composer()?.disabled, true, "an unread or revoked identity still prevents signing new input")
    currentWorkspace.canSaveMessages = true
    currentWorkspace.data = { ...context, conversation: { ...context.conversation, owner: 'away' } }
    assert.equal(app.render('ConversationScreen').some(({ props }) => String(props.title).includes('离线，对话在等它')), true, 'an unverified local provider still offers to take over from an offline owner')
  } finally { app.close() }
})

test("an owner's reply on its way keeps its streamed text until the reply lands", () => {
  const job = { scope: { kind: 'main' as const }, trigger: "message:m1", state: "publishing", message_id: "m1", reply_message_id: "r1", source_node: "owner" }
  const ask = { message_id: "m1", event_hash: "m1", role: "user", text: "hi", created_at_ms: 1 }
  const progress = { kind: "manager", key: "live", conversation_id: "chat", trigger: "message:m1", source_node: "owner", sequence: 1, text: "partial answer", items: [], needsRefresh: false }
  let messages: object[] = [ask]
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), data: { ...parseConversation(context, "chat"), messages, scheduler: { scope_jobs: [], jobs: [job], unanswered: [] } }, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [progress], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    refresh: async () => {},
  }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const streamed = () => (app.render('ConversationScreen').find(({ type }) => type === 'Chat')?.props.entries as { message?: { state?: string; text: string } }[]).filter((entry) => entry.message?.state === 'streaming').map((entry) => entry.message?.text).join()
  try {
    assert.equal(streamed(), "partial answer")
    messages = [ask, { message_id: "r1", event_hash: "r1", role: "assistant", text: "full answer", created_at_ms: 2, in_reply_to: "m1" }]
    assert.equal(streamed(), "")
  } finally { app.close() }
})

test("a closed question neither waits on the person nor hides 思考中", () => {
  const job = { scope: { kind: 'main' as const }, trigger: "message:m1", state: "running", message_id: "m1" }
  const ask = { message_id: "m1", event_hash: "m1", role: "user", text: "hi", created_at_ms: 1 }
  const question = (question_id: string, closed: string | null) => ({ question_id, conversation_id: "chat", bot: null, severity: "attention", text: "允许吗？", choices: [],
    about: { node: "gateway", agent: "codex", session_id: "s", cwd: "/w", summary: "", request_id: "r" }, author: "gateway", created_at_ms: 2, answer: null, closed })
  let questions = [question("lost", "request_lost")]
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), data: { ...parseConversation(context, "chat"), messages: [ask], questions, scheduler: { scope_jobs: [], jobs: [job], unanswered: [] } }, pendingMessages: [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true, answered: {},
    refresh: async () => {},
  }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const thinking = () => (app.render('ConversationScreen').find(({ type }) => type === 'Chat')?.props.entries as { key: string }[]).some((entry) => entry.key === 'activity')
  try {
    assert.equal(thinking(), true, "a question its device closed is not the bot's status")
    questions = [question("open", null)]
    assert.equal(thinking(), false, "an open question is the status already")
  } finally { app.close() }
})

test('offline chat keeps reconnect and stale waiting summary above the composer, then drops the stale prefix when back online', () => {
  let connectionOnline = false
  let retried = 0
  const question = { question_id: 'wait-q', conversation_id: 'chat', bot: null, severity: 'attention' as const, text: '继续吗？', choices: [{ label: '继续' }],
    about: { node: 'gateway', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' }, author: 'gateway', created_at_ms: 1, answer: null, closed: null }
  const workspace = () => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), pendingMessages: [], canSaveMessages: connectionOnline, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', name: '本机', online: true, local: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }],
    liveProgress: [], interrupted: [], writable: connectionOnline, selectedBot: parityBot, connectionOnline, answered: {},
    data: { ...parseConversation(context, 'chat'), questions: [question], scheduler: null, messages: [] },
    refresh: async () => {},
  })
  const harness = conversationHarness(workspace, { retryNow: () => { retried++ } })
  try {
    const above = componentText(harness.composer()?.above)
    assert.match(above, /重试连接/)
    assert.match(above, /最近记录 · /)
    assert.ok(harness.composer()?.disabled)
    const reconnect = harness.findAction('重试连接')?.onClick
    assert.equal(typeof reconnect, 'function')
    if (typeof reconnect === 'function') reconnect()
    assert.equal(retried, 1)
    connectionOnline = true
    assert.doesNotMatch(componentText(harness.composer()?.above), /最近记录 · /)
    assert.match(componentText(harness.composer()?.above), /待处理/)
    assert.equal(harness.composer()?.disabled, false)
  } finally { harness.app.close() }
})

test('merged and in-flight user messages show honest status labels and refresh-only 再查一次', async () => {
  let refreshed = 0, sent = 0
  let connectionOnline = true
  const baseAsk = { message_id: 'm0', event_hash: 'm0', role: 'user' as const, text: 'first', created_at_ms: 0 }
  const build = (jobs: object[], messages: object[], pending: PendingMessage | null = null) => ({
    ...historyActions, generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), pendingMessages: pending ? [pending] : [], canSaveMessages: true, workspaceState: null,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', online: connectionOnline, local: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }],
    liveProgress: [], interrupted: [], writable: true, selectedBot: parityBot, connectionOnline,
    data: { ...parseConversation(context, 'chat'), messages, scheduler: { jobs, unanswered: [], capabilities: { streaming: true, cancel: true, steer: true } } },
    refresh: async () => { refreshed++ }, sendMessage: async () => { sent++; return true },
  })
  let current = build([], [], { report_results: true, message_id: 'pending', text: 'sending', conversation_id: 'chat', target: { kind: 'bot_main', bot_id: parityBot.bot_id }, intent: 'continue', sequence: 1, created_at_ms: 1, delivery: 'submitting' })
  const harness = conversationHarness(() => current)
  try {
    assert.match(componentText(harness.messageTree('pending')), /正在确认送达/)
    current = build(
      [{ trigger: 'message:m0', state: 'running', message_id: 'm0' }, { trigger: 'message:m1', state: 'queued', message_id: 'm1' }],
      [baseAsk, { message_id: 'm1', event_hash: 'm1', role: 'user', text: 'queued', created_at_ms: 1 }],
    )
    assert.match(componentText(harness.messageTree('m1')), /已排队/)
    for (const [outcome, label] of [['answered', '关联回合已完成'], ['unfinished', '关联回合未完成'], ['unknown', '补充结果未知']] as const) {
      current = build([{ trigger: 'message:m2', state: 'steered', message_id: 'm2', outcome }], [{ message_id: 'm2', event_hash: 'm2', role: 'user', text: outcome, created_at_ms: 2 }])
      const tree = harness.messageTree('m2')
      assert.ok(componentText(tree).includes(label))
      if (outcome === 'unknown') {
        refreshed = 0; sent = 0
        const action = findActionProps(tree, '再查一次')
        assert.ok(action?.action)
        await (action.action as () => Promise<void>)()
        assert.equal(refreshed, 1)
        assert.equal(sent, 0)
        connectionOnline = false
        current = { ...current, connectionOnline }
        assert.equal(findActionProps(harness.messageTree('m2'), '再查一次')?.disabled, true)
      }
    }
  } finally { harness.app.close() }
})

test('queued input edits and cancellation use the exact input CAS and leave the chat draft alone', async () => {
  const target: InputTarget = { kind: 'bot_main', bot_id: parityBot.bot_id }
  const queued: WorkspaceDomainState['inputs'][number] = { input: { report_results: true, message_id: 'queued', target, intent: 'defer', sequence: 1, text: 'original queued text' }, conversation_id: 'chat', source_event: 'source', control_event: 'control', effective_order: 1, deferred: true, cancelled: false }
  const controls: unknown[] = [], queries: unknown[] = []
  let stage: InputReceipt['stage'] = 'entry_accepted', error: unknown = null
  const shared = { ...parityShared(), workspaceState: { channels: [], tasks: [], assignments: [], inputs: [queued], coverage: { limit: 128, truncated: false } },
    queryInput: async (request: unknown) => { queries.push(JSON.parse(JSON.stringify(request))); return { stage } },
    controlInput: async (request: unknown) => { controls.push(JSON.parse(JSON.stringify(request))) },
    readWorkspaceState: async () => {},
    cancelManager: async () => { throw new Error('an input cannot cancel the manager job') },
    sendMessage: async () => { throw new Error('editing must not send a replacement message') },
  }
  const app = componentHarness('./workspace-chat.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
    '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error, run: async (_key: string, action: () => Promise<void>) => { try { await action() } catch (cause) { error = cause } } }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
  })
  const render = () => app.render('InputQueue', { target })
  const press = async (label: string) => { const button = render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label); assert.ok(button && typeof button.props.onClick === 'function'); button.props.onClick(); await settleComponent() }
  try {
    await press('编辑输入')
    const edit = render().find(({ type }) => type === 'Textarea')
    assert.ok(edit && typeof edit.props.onChange === 'function')
    edit.props.onChange({ currentTarget: { value: 'edited queued text' } })
    await press('保存输入修改')
    assert.deepEqual(queries, [{ message_id: 'queued', target, conversation_id: 'chat' }])
    assert.deepEqual(controls, [{ message_id: 'queued', expected_event: 'control', action: { kind: 'edit', text: 'edited queued text', attachments: [] } }])
    await press('取消稍后输入')
    assert.deepEqual(controls.at(-1), { message_id: 'queued', expected_event: 'control', action: { kind: 'cancel' } })
    stage = 'unknown'
    await press('取消稍后输入')
    assert.equal(controls.length, 2, 'a changed receipt cannot authorize another control write')
    assert.match(String(error), /可能已提交/)
  } finally { app.close() }
})

test('task detail keeps long progress collapsed by default and finished tasks keep their result over stale progress', async () => {
  const work = parityWork({ state: 'finished', outcome: 'completed', result_text: '最终结果', summary: '✓ 读记录\n→ 核对结果\n· 等待同步' })
  const workspace = {
    ...parityShared(), readWork: async () => ({ work: { ...work, conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } }),
    generation: 1, connectionOnline: true, writable: true, incomplete: false, selectedBot: parityBot, nodes: [parityNode], answered: {},
    data: parseConversation({ ...context, works: [parityWork({ state: 'finished', outcome: 'completed', result_text: '最终结果', summary: '✓ 读记录\n→ 核对结果\n· 等待同步' })] }),
    liveProgress: [{ kind: 'native', work_id: 'work', target_node: 'worker', text: '旧进展行', sequence: 1 }],
  }
  const app = componentHarness('./task-detail.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_kind: string, action: () => Promise<void>) => action() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/lib/errors.ts': errors,
    '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/lib/node.ts': { providerNames },
  }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  try {
    app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    await settleComponent()
    const elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.equal(elements.some(({ props }) => props.text === '最终结果'), true)
    assert.doesNotMatch(componentText(elements), /旧进展行/)
    const steps = elements.find(({ props }) => props.label === '最近步骤摘要 · 3')
    assert.equal(steps?.props.open, false)
    assert.equal(elements.some(({ props }) => props.label === '详细进展'), false)
  } finally { app.close() }
})

test('unknown task detail reads its exact work without starting a Main request and keeps manual settlement available', async () => {
  let navigated = false, sent = 0, refreshed = 0
  const reads: unknown[] = []
  let connectionOnline = true
  const work = parityWork({ state: 'unverified', outcome: 'unverified', failure_reason: '无法证明结果' })
  const workspace = {
    ...parityShared(),
    readWork: async (input: unknown) => { reads.push(JSON.parse(JSON.stringify(input))); return { work: { ...work, conversation_id: 'chat' }, coverage: { source: 'shared_history', complete: false } } },
    refresh: async () => { refreshed++ },
    ...historyActions, generation: 1, isCurrent: () => true, connectionOnline, writable: true, incomplete: false, selectedBot: parityBot, nodes: [parityNode], answered: {},
    data: parseConversation({ ...context, works: [work] }),
    liveProgress: [],
    sendMessage: async () => { sent++; return false },
    resolveCheck: async () => {},
  }
  const app = componentHarness('./task-detail.tsx', {
    '@tanstack/react-router': { useNavigate: () => () => { navigated = true }, Link: 'Link' },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: (_kind: string, action: () => Promise<void>) => action() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/lib/errors.ts': errors,
    '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/lib/node.ts': { providerNames },
    './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
  }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  try {
    app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    await settleComponent()
    let elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.match(componentText(elements), /还不知道做完没有/)
    const ask = findActionProps(elements, '先查结果')
    assert.ok(ask?.action)
    assert.equal(ask.disabled, false)
    await (ask.action as () => Promise<void>)()
    assert.equal(sent, 0)
    assert.equal(refreshed, 1)
    assert.equal(navigated, false)
    app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    await settleComponent()
    assert.deepEqual(reads, [{ conversation_id: 'chat', work_id: 'work' }, { conversation_id: 'chat', work_id: 'work' }])
    workspace.connectionOnline = false
    elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.equal(findActionProps(elements, '先查结果')?.disabled, true)
    assert.equal(elements.some(({ props }) => props.label === '我已自行核对'), true)
  } finally { app.close() }
})

test("an outstanding clipboard permission cannot keep the add-device sheet busy or release another action", async () => {
  let generation = 1, creation = pendingResult()
  const invitation = { link: 'vibeface://join/vibeface1test', expiresAt: Date.now() + 600_000 }
  const clipboard = pendingResult()
  const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, principal: { can_manage: true, network_id: 'network' }, entry: { kind: 'gateway', address: 'https://node.example', device_id: 'target' } } }
  const device = { connectionGeneration: () => generation, ready: ready(), joins: [], createInvitation: () => creation.promise, refreshDirectory: async () => {} }
  const app = componentHarness('../devices/add-device.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { label: '安全码', icon: 'ScanEye' } } },
    '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message },
  }, { navigator: { clipboard: { writeText: () => clipboard.promise } }, setInterval: () => 0, clearInterval: () => {} })
  const render = () => app.render('AddDevice')
  const labelled = (label: string) => render().find(({ props }) => [props.children].flat().includes(label))?.props
  try {
    render()
    labelled('生成链接')?.onClick?.(); creation.resolve(invitation); await settleComponent()
    labelled('复制链接')?.onClick?.()
    assert.equal(labelled('复制链接')?.disabled, true)
    generation++; device.ready = ready(); render(); render()
    assert.equal(labelled('生成链接')?.disabled, false)
    creation = pendingResult(); labelled('生成链接')?.onClick?.()
    clipboard.resolve(undefined); await settleComponent()
    assert.equal(labelled('生成链接')?.disabled, true)
    creation.resolve(invitation); await settleComponent()
    assert.equal(labelled('复制链接')?.disabled, false)
  } finally { app.close() }
})

test("the invitation's QR code is its link, for the app", async () => {
  const shown = async (link: string) => {
    const encoded: string[] = []
    const device = { connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true, network_id: 'network' }, entry: { kind: 'gateway', address: 'http://127.0.0.1:4790', device_id: 'target' } }, joins: [], createInvitation: async () => ({ link, expiresAt: Date.now() + 600_000 }), refreshDirectory: async () => {} }
    const app = componentHarness('../devices/add-device.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { label: '安全码', icon: 'ScanEye' } } },
      '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message }, 'qrcode-generator': () => ({ addData(value: string) { encoded.push(value) }, make() {}, createDataURL: () => 'data:qr' }),
    }, { setInterval: () => 0, clearInterval: () => {} })
    try {
      app.render('AddDevice'); app.render('AddDevice').find(({ props }) => [props.children].flat().includes('生成链接'))?.props.onClick?.(); await settleComponent()
      return { tree: app.render('AddDevice'), encoded }
    } finally { app.close() }
  }
  const local = await shown('vibeface://join/vibeface1test')
  assert.deepEqual(local.encoded, ['vibeface://join/vibeface1test'])
  assert.equal(local.tree.find(({ type }) => type === 'img')?.props.alt, '用手机 App 扫码')
  assert.ok(local.tree.some(({ type, props }) => type === 'p' && [props.children].flat().includes('用手机 App 扫码')))
})

test("an entry is stored in the one form Nodes send it in, and a LAN entry as its device id and addresses", () => {
  const id = "ab".repeat(32)
  for (const value of [{ address: "https://node.example", device_id: id }, { device_id: id, addrs: ["192.168.1.4:7001", "[fd00::4]:7001"] }]) assert.deepEqual(entryJson(parseEntry(value)), value)
  assert.deepEqual(parseEntry({ device_id: id, addrs: ["192.168.1.4:7001", "node.example:7001"] }), { kind: "lan", addrs: ["192.168.1.4:7001"], device_id: id })
  assert.throws(() => parseEntry({ device_id: id, addrs: ["node.example:7001"] }), /访问地址缺失或格式无效/)
})

test("device id boundaries require the device_id field", () => {
  const id = "ab".repeat(32), other = "cd".repeat(32)
  const parsers = { parseEntry, parseInfo }
  assert.equal(parsers.parseEntry({ address: "https://node.example", device_id: id }).device_id, id)
  for (const relay of [false, true]) {
    const entry = parsers.parseEntry({ address: "https://node.example", device_id: id, relay })
    assert.equal(entry.device_id, id)
    assert.equal(Object.hasOwn(entry, "relay"), false)
  }
  assert.equal(parsers.parseInfo({ name: "node", version: "1", initialized: true, network_id: other, device_id: id }).device_id, id)
  const guest = parsers.parseInfo({ name: "Node", version: "1", initialized: true, network_id: null, device_id: id })
  assert.equal(guest.network_id, null)
  assert.equal(guest.connection_entry, undefined)
  assert.equal(parseNodes({ nodes: [{ local: true, online: true, revoked: false, device_id: id }] })[0].device_id, id)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", node_id: id }), /设备地址无效/)
  assert.throws(() => parsers.parseInfo({ name: "node", version: "1", initialized: true, network_id: other, node_id: id }), /设备地址无效/)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", device_id: "invalid" }), /设备地址无效/)
  assert.throws(() => parseNodes({ nodes: [{ local: true, online: true, revoked: false, node_id: id }] }), /缺少文字/)
})

function nativeFileFixture() {
  const index = new Map<string, string>()
  const files = new Map<string, Uint8Array>(), directories = new Set<string>(['file:///documents', 'file:///cache'])
  let failure = ''
  const uriOf = (...parts: (string | { uri: string })[]) => parts.map(part => typeof part === 'string' ? part : part.uri).join('/').replace(/\/$/, '')
  class Directory {
    readonly uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(...parts) }
    get exists() { return directories.has(this.uri) }
    create() { directories.add(this.uri) }
    list() {
      const direct = (uri: string) => uri.startsWith(this.uri + '/') && !uri.slice(this.uri.length + 1).includes('/')
      return [...[...directories].filter(direct).map(uri => new Directory(uri)), ...[...files.keys()].filter(direct).map(uri => new File(uri))]
    }
    delete() {
      if (failure === this.uri) throw new Error('controlled delete failure')
      for (const uri of files.keys()) if (uri.startsWith(this.uri + '/')) files.delete(uri)
      directories.delete(this.uri)
    }
  }
  class File {
    uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(...parts) }
    get exists() { return files.has(this.uri) }
    get name() { return this.uri.split('/').at(-1) ?? '' }
    get size() { return files.get(this.uri)?.length ?? 0 }
    create() { files.set(this.uri, new Uint8Array()) }
    writeSync(value: string | Uint8Array) { if (failure === this.uri || failure === 'recovery-write' && this.uri.endsWith('.tmp')) throw new Error('controlled write failure'); files.set(this.uri, typeof value === 'string' ? new TextEncoder().encode(value) : value.slice()) }
    async write(value: string | Uint8Array) { this.writeSync(value) }
    textSync() { return new TextDecoder().decode(files.get(this.uri)) }
    async text() { return this.textSync() }
    async bytes() { return files.get(this.uri)?.slice() ?? new Uint8Array() }
    copySync(destination: File) { const value = files.get(this.uri); assert.ok(value); destination.writeSync(value) }
    moveSync(destination: File) { this.copySync(destination); files.delete(this.uri); this.uri = destination.uri }
    delete() { if (failure === this.uri) throw new Error('controlled delete failure'); files.delete(this.uri) }
  }
  const native = { Directory, File, Paths: { document: new Directory('file:///documents'), cache: new Directory('file:///cache') } }
  const mmkv = { createMMKV: () => ({ getString: (key: string) => index.get(key), getAllKeys: () => [...index.keys()], set: (key: string, value: string) => { if (failure === 'recovery-index') throw new Error('controlled index failure'); index.set(key, value) }, remove: (key: string) => index.delete(key), clearAll: () => index.clear() }) }
  return { native, mmkv, files, fail: (uri: string) => { failure = uri } }
}

test('native private files preserve sealed records and external originals while erase fences delayed pickers and retries failed deletes', async () => {
  const fixture = nativeFileFixture()
  const app = componentHarness('../../../../../apps/mobile/src/platform/files.ts', { 'expo-file-system': fixture.native, 'react-native-mmkv': fixture.mmkv, '@vibeface/client/lib/node.ts': nodeSettings }, { crypto, TextDecoder, Uint8Array })
  const { privateFiles } = app.module as typeof import('../../../../../apps/mobile/src/platform/files.ts')
  const source = new fixture.native.File('file:///provider/original.json'), exported = new fixture.native.File('file:///export/kept.json')
  source.writeSync('original'); exported.writeSync('external export')
  const envelope = JSON.stringify({ version: 1, network_id: 'a'.repeat(64), envelope: { header: { format: 'vibeface-recovery', version: 1, kdf: 'argon2id', memory_kib: 19456, iterations: 2, parallelism: 1, salt: '1'.repeat(32), cipher: 'xchacha20poly1305', nonce: '2'.repeat(48) }, ciphertext: '3'.repeat(64) } })
  const earlier = new fixture.native.File('file:///documents/vibeface-recovery-ABCDEF12-2026-11-08.json')
  const similar = new fixture.native.File('file:///documents/vibeface-recovery-ABCDEF12-2026-11-08-notes.json')
  const nonRecovery = new fixture.native.File('file:///documents/vibeface-recovery-ABCDEF13-2026-11-08.json')
  const externalRecovery = new fixture.native.File('file:///export/vibeface-recovery-ABCDEF12-2026-11-08.json')
  earlier.writeSync(envelope); similar.writeSync(envelope); nonRecovery.writeSync('{"user":"notes"}'); externalRecovery.writeSync(envelope)
  privateFiles.recoveryStorage.write('sealed copy with index')
  const reloaded = componentHarness('../../../../../apps/mobile/src/platform/files.ts', { 'expo-file-system': fixture.native, 'react-native-mmkv': fixture.mmkv, '@vibeface/client/lib/node.ts': nodeSettings }, { crypto, TextDecoder, Uint8Array })
  assert.equal((reloaded.module as typeof import('../../../../../apps/mobile/src/platform/files.ts')).privateFiles.recoveryStorage.read(), 'sealed copy with index')
  fixture.fail('recovery-write')
  assert.throws(() => privateFiles.recoveryStorage.write('not committed'), /controlled write failure/)
  assert.equal(privateFiles.recoveryStorage.read(), 'sealed copy with index')
  fixture.fail('recovery-index')
  assert.throws(() => privateFiles.recoveryStorage.write('not indexed'), /controlled index failure/)
  assert.equal(privateFiles.recoveryStorage.read(), 'sealed copy with index')
  fixture.fail('')
  const picked = privateFiles.retain(source.uri)
  assert.notEqual(picked.uri, source.uri)
  const nativeCopy = new fixture.native.File('file:///cache/DocumentPicker/copied.json')
  nativeCopy.writeSync('picker cache')
  fixture.fail(nativeCopy.uri)
  assert.throws(() => privateFiles.releasePicker(nativeCopy.uri), /controlled delete failure/)
  await assert.rejects(privateFiles.erase(async () => {}), /controlled delete failure/)
  assert.equal(picked.exists, true, 'failed cleanup retains retry evidence and owned copies')
  fixture.fail('')
  const delayed = pendingResult()
  let returned = false, erased = false
  const work = privateFiles.run(async current => {
    await delayed.promise
    nativeCopy.writeSync('late picker cache')
    try { if (current()) { privateFiles.retain(nativeCopy.uri); returned = true } }
    finally { privateFiles.releasePicker(nativeCopy.uri) }
  })
  const rejected = assert.rejects(work, /正在清除/)
  const erase = privateFiles.erase(async () => { erased = true })
  await settleComponent()
  assert.equal(erased, false)
  await assert.rejects(privateFiles.run(async () => {}), /正在清除/)
  delayed.resolve(undefined)
  await Promise.all([erase, rejected])
  assert.equal(returned, false)
  assert.equal(erased, true)
  assert.equal(nativeCopy.exists, false)
  assert.equal(picked.exists, false)
  assert.equal(privateFiles.recoveryStorage.read(), null)
  assert.equal(source.textSync(), 'original')
  assert.equal(exported.textSync(), 'external export')
  assert.equal(earlier.exists, false, 'the earlier writer-owned root copy is erased')
  assert.equal(similar.textSync(), envelope, 'a similar user filename is preserved')
  assert.equal(nonRecovery.textSync(), '{"user":"notes"}', 'a matching name without a sealed recovery shape is preserved')
  assert.equal(externalRecovery.textSync(), envelope, 'an external same-name exported recovery is preserved')
})

test('a missing native or web recovery copy never enables external-save confirmation', () => {
  for (const [path, name, saveProp] of [
    ['../../../../../apps/mobile/src/features/recovery/recovery.tsx', 'SaveRecoveryFile', 'save'],
    ['../identity/recovery-controls.tsx', 'SaveRecoveryFile', 'download'],
  ]) {
    const app = componentHarness(path, {
      '@vibeface/client/identity/copy.ts': { recoveryCopy: { why: '', onlyCopy: '', keptApart: '文件和密码分开保存' } },
      '@vibeface/client/lib/secure.ts': { recoveryFileStem: (code: string) => code },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@/components/app/kit': { claimFocus() {}, InlineError: 'InlineError' },
    }, {}, path.includes('/mobile/') ? 'SaveRecoveryFile' : '')
    try {
      const missing = app.render(name, { code: 'ABCDEF12', saved: false, onSaved() {} })
      const confirmation = missing.find(({ props }) => props.type === 'checkbox' || props.accessibilityLabel === '文件和密码分开保存')
      assert.equal(confirmation?.props.disabled, true)
      const handed = app.render(name, { code: 'ABCDEF12', saved: false, onSaved() {}, [saveProp]: { done: true, run() {} } })
      assert.equal(handed.find(({ props }) => props.type === 'checkbox' || props.accessibilityLabel === '文件和密码分开保存')?.props.disabled, false)
      const confirmed = app.render(name, { code: 'ABCDEF12', saved: true, onSaved() {} })
      assert.equal(confirmed.find(({ props }) => props.type === 'checkbox' || props.accessibilityLabel === '文件和密码分开保存')?.props.disabled, false)
    } finally { app.close() }
  }
})

test('native system notification events keep typed destinations and clear navigation on erase', async () => {
  const fixture = nativeFileFixture()
  const owned = componentHarness('../../../../../apps/mobile/src/platform/files.ts', { 'expo-file-system': fixture.native, 'react-native-mmkv': fixture.mmkv, '@vibeface/client/lib/node.ts': nodeSettings }, { crypto, TextDecoder, Uint8Array })
  const { privateFiles } = owned.module as typeof import('../../../../../apps/mobile/src/platform/files.ts')
  type Response = { notification: { date: number; request: { identifier: string; content: { data: { destination: NotificationDestination } } } }; actionIdentifier: string }
  let response: ((value: Response) => void) | undefined
  let dismissed = 0, forgotten = 0
  const storage = new Map<string, string>()
  const scheduled: unknown[] = []
  const app = componentHarness('../../../../../apps/mobile/src/platform/index.ts', {
    'react-native': { Platform: { OS: 'ios', isPad: false }, AppState: { currentState: 'active', addEventListener: () => ({ remove() {} }) } },
    'expo-file-system': fixture.native,
    'expo-notifications': {
      setNotificationHandler() {}, AndroidImportance: { HIGH: 1 }, setNotificationChannelAsync: async () => {},
      addNotificationResponseReceivedListener(listener: typeof response) { response = listener; return { remove() { response = undefined } } },
      getLastNotificationResponseAsync: async () => null, clearLastNotificationResponseAsync: async () => {},
      scheduleNotificationAsync: async (value: unknown) => { scheduled.push(value); return 'scheduled' },
      dismissAllNotificationsAsync: async () => { dismissed++ },
    },
    'react-native-mmkv': { createMMKV: () => ({ getString: (key: string) => storage.get(key), set: (key: string, value: string) => storage.set(key, value), clearAll: () => storage.clear(), getAllKeys: () => [...storage.keys()] }) },
    '@vibeface/secure-native': { lan: {}, secure: {} },
    '@vibeface/client/lib/transport.ts': { createTransport: () => ({ forgetClient: async () => { forgotten++ } }) },
    '@vibeface/client/identity/copy.ts': { lostKeyText: () => '' },
    './files': { privateFiles },
    './notification-opens': { notificationOpenings },
  })
  const { mobilePlatform } = app.module as typeof import('../../../../../apps/mobile/src/platform/index.ts')
  const destination: NotificationDestination = { network_id: 'network-fixture', device_id: 'device-fixture', notification_id: 'notification-fixture', conversation_id: 'conversation-fixture' }
  const opened: NotificationDestination[] = []
  const unwatch = mobilePlatform.notifier.watchOpen?.(async value => { opened.push(value); return true })
  mobilePlatform.notifier.show({ title: 'fixture', body: 'fixture', tag: 'same', destination }, () => {})
  await privateFiles.run(async () => {})
  const event: Response = { notification: { date: 1, request: { identifier: 'same', content: { data: { destination } } } }, actionIdentifier: 'open' }
  response?.(event); response?.(event)
  await new Promise<void>(resolve => setImmediate(resolve))
  assert.deepEqual(opened, [destination])
  assert.deepEqual(JSON.parse(JSON.stringify(scheduled)), [{ identifier: 'same', content: { title: 'fixture', body: 'fixture', data: { destination } }, trigger: { channelId: 'vibeface' } }])
  await mobilePlatform.erase?.()
  response?.({ ...event, notification: { ...event.notification, date: 2 } })
  await new Promise<void>(resolve => setImmediate(resolve))
  assert.deepEqual(opened, [destination], 'erase removes the navigation consumer')
  assert.equal(dismissed, 1)
  assert.equal(forgotten, 1)
  unwatch?.()
})

test('the native switch confirmation fixes the decoded entry and never erases for a changed link or cancellation', async () => {
  const entryId = 'a'.repeat(64), link = 'vibeface://join/vibeface1fixture'
  let url = link, erased = 0, canceled = 0
  const kept: (string | null)[] = [], failures: unknown[] = []
  const app = componentHarness('../../../../../apps/mobile/app/switch-network.tsx', {
    'expo-router': { router: { back: () => { canceled++ } }, useLocalSearchParams: () => ({ link }) },
    'expo-linking': { useLinkingURL: () => url },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: { members: [] }, linkTarget: () => 'other' }) },
    '@vibeface/client/identity/copy.ts': { soleManager: () => false, switchCopy: { title: '换到新的网络？', erase: () => '清除应用内数据', confirm: '换网络' } },
    '@vibeface/client/identity/switch-network.ts': { joinDestination },
    '@/platform': { mobilePlatform: { secure: { ticketDecode: () => JSON.stringify({ entry_id: entryId }) } }, self: '这台手机' },
    '@/lib/join-link': { openedLink: (value: string | null) => value?.startsWith('vibeface://join/') && joinTicket(value) ? value : null, keepSwitchedLink: (value: string | null) => kept.push(value) },
    '@/lib/factory-reset': { factoryReset: async () => { erased++ } },
    '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_name: string, action: () => Promise<void>) => { try { await action() } catch (cause) { failures.push(cause) } } }) },
  })
  try {
    app.render('default')
    assert.match(componentText(app.output), new RegExp(entryId))
    const cancel = app.render('default').find(({ props }) => props.variant === 'ghost')?.props.onPress
    assert.ok(typeof cancel === 'function'); cancel()
    assert.equal(canceled, 1); assert.equal(erased, 0)
    url = 'vibeface://join/vibeface1different'
    const changed = app.render('default').find(({ props }) => props.variant === 'destructive')?.props
    assert.equal(changed?.disabled, true)
    assert.ok(typeof changed?.onPress === 'function'); changed.onPress(); await settleComponent()
    assert.equal(erased, 0); assert.equal(failures.length, 1)
    url = link
    const confirmed = app.render('default').find(({ props }) => props.variant === 'destructive')?.props
    assert.equal(confirmed?.disabled, false)
    assert.ok(typeof confirmed?.onPress === 'function'); confirmed.onPress(); await settleComponent()
    assert.equal(erased, 1); assert.deepEqual(kept, [link])
  } finally { app.close() }
})


test('Web and Mobile expose every omitted section, fetch only on demand and offer cancellation', async () => {
  for (const platform of ['web', 'mobile']) {
    const calls: string[] = [], cancellations: string[] = []
    const shared = { ...parityShared(), data: parseConversation({ ...context, history: Object.fromEntries(workspaceModel.historySections.map((section) => [section, { next_cursor: `${section}-cursor`, complete: false }])) }),
      loadHistory: async (section: HistorySection) => { calls.push(section) }, cancelHistory: (section: HistorySection) => { cancellations.push(section) },
    }
    const controls = componentHarness(platform === 'web' ? '../chat/history-controls.tsx' : '../../../../../apps/mobile/src/features/chat/history-controls.tsx', {})
    const render = () => controls.render('HistoryControls', { workspace: shared })
    const buttons = () => render().filter((element) => element.type === 'Button')
    const press = (button: ReturnType<typeof buttons>[number]) => {
      const action = platform === 'web' ? button.props.onClick : button.props.onPress
      assert.equal(typeof action, 'function')
      if (typeof action === 'function') action()
    }
    try {
      assert.equal(buttons().length, 19)
      assert.deepEqual(calls, [], 'opening a bounded snapshot never fetches all history')
      const messages = buttons().find((button) => componentText(button.props.children) === '读取更多聊天消息')
      assert.ok(messages); press(messages); await settleComponent()
      assert.deepEqual(calls, ['messages'])
      shared.historyLoading = new Set<HistorySection>(['messages'])
      const cancel = buttons().find((button) => componentText(button.props.children) === '取消读取聊天消息')
      assert.ok(cancel); press(cancel)
      assert.deepEqual(cancellations, ['messages'])
      shared.connectionOnline = false
      assert(buttons().filter((button) => componentText(button.props.children).startsWith('读取更多')).every((button) => button.props.disabled === true))
      shared.data.history.messages = { complete: true, next_cursor: null }
      assert.equal(buttons().length, 18)
    } finally { controls.close() }
  }
})

test('the Web chat keeps the first visible identity and offset through older pages, pushes and streaming updates', () => {
  const row = (key: string) => ({ key, message: { message_id: key, role: 'user', text: key, event_hash: key, created_at_ms: 1 } })
  let positions = [{ key: 'one', top: 0 }, { key: 'two', top: 200 }, { key: 'three', top: 400 }]
  const view = { scrollTop: 200, scrollHeight: 1000, clientHeight: 200, getBoundingClientRect: () => ({ top: 0 }),
    querySelectorAll: () => positions.map((item) => ({ dataset: { historyKey: item.key }, getBoundingClientRect: () => ({ top: item.top - view.scrollTop, bottom: item.top + 100 - view.scrollTop }) })), querySelector: () => ({}),
  }
  const chat = componentHarness('../chat/elements/chat.tsx', { '@vibeface/client/chat/timeline.ts': { messageTimes }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } }, { ResizeObserver: class { observe() {} disconnect() {} }, matchMedia: () => ({ matches: true }) })
  const render = (keys: string[]) => chat.render('Chat', { entries: keys.map(row), empty: null })
  try {
    const viewport = render(['one', 'two', 'three']).find((element) => element.type === 'MessageScrollerViewport')
    assert.ok(viewport)
    workspaceModel.record(viewport.props.ref).current = view
    assert.equal(typeof viewport.props.onScroll, 'function')
    if (typeof viewport.props.onScroll === 'function') viewport.props.onScroll()
    positions = [{ key: 'older', top: 0 }, { key: 'one', top: 150 }, { key: 'two', top: 350 }, { key: 'three', top: 550 }]
    let elements = render(['older', 'one', 'two', 'three'])
    assert.equal(view.scrollTop, 350)
    assert.equal(elements.find((element) => element.type === 'MessageScrollerProvider')?.props.autoScroll, false)
    positions.push({ key: 'pushed', top: 750 }); view.scrollHeight = 1200
    render(['older', 'one', 'two', 'three', 'pushed'])
    assert.equal(view.scrollTop, 350)
    positions = positions.map((item) => item.key === 'two' || item.key === 'three' || item.key === 'pushed' ? { ...item, top: item.top + 40 } : item)
    elements = render(['older', 'one', 'two', 'three', 'pushed'])
    assert.equal(view.scrollTop, 390)
    assert.equal(elements.find((element) => element.type === 'MessageScrollerProvider')?.props.autoScroll, false)
  } finally { chat.close() }
})
