import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import { entryAddress, entryJson, entryKey, messageOf, parseEntry, parseInfo, providerNames } from "@vibeface/client/lib/node.ts"
import * as nodeSettings from "@vibeface/client/lib/node.ts"
import type { NodeSettingsResponse, ProviderReadiness, ProviderStatus, ReadyConnection } from "@vibeface/client/lib/node.ts"
import type { WorkRecord } from "@vibeface/client/workspace/types.ts"
import { cardState, nodeName, taskGroup } from "@vibeface/client/workspace/labels.ts"
import { botReadiness, inputText, parseArtifactPage, parseConversation, pendingChecks, parseNodes, parseCreateBot, parseBot, formatSize, attachmentName, signedOrder } from "@vibeface/client/workspace/model.ts"
import { botCopy, folderLabels, folderSetup, placeChanges, projectChoices, projectKey, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { agentName, answerLabel, attentionLabel, approvalTask, awaitsAnswer, declined, foldAnswers, projectName, questionMeta, questionOpen, settledAnswer, sharedProject, questionParts } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { followUp, followUpCopy, promptSummary } from "@vibeface/client/workspace/labels.ts"
import { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { deviceCopy, setUpOnManager } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread, unreadCounts } from "@vibeface/client/chat/timeline.ts"
import { accessOf } from "@vibeface/client/identity/access.ts"
import { storageOf } from "@vibeface/client/workspace/storage.ts"
import { unreachable } from "@vibeface/client/workspace/home.ts"
import { primaryLane } from "@vibeface/client/identity/connection-lane.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"
import { inboxItems } from "@vibeface/client/workspace/inbox.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test", owner: "gateway", owner_epoch: 1 }, messages: [],
  works: [], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], questions: [], sessions: [],
}
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
  const work = { work_id: "work", trigger: "message:one", target_node: "worker", cwd: "/project", title: "检查项目", prompt: "检查项目", state: "running", summary: "进行中", outcome: null, result_text: "", failure_reason: null, resolved: { provider: "codex", model: null } }
  const scheduler = { jobs: [{ trigger: "message:one", state: "queued", message_id: "one" }], unanswered: [], capabilities: { streaming: true, cancel: false, steer: false } }
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
  assert.equal(parseConversation({ ...context, scheduler: { jobs: [{ trigger: "message:two", owner: "node", conversation_id: "c", state: "pending_delivery", message_id: "two" }], unanswered: [] } }).scheduler?.jobs[0].state, "pending_delivery")
  assert.throws(() => parseConversation({ ...context, scheduler: { jobs: [] } }), "the unanswered list is always sent")
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
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  let cursor = 0
  const afterRender: (() => void)[] = []
  const react = {
    createContext: () => ({}), useContext: () => null,
    useState(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = initial
      return [values[index], (next: unknown) => { values[index] = typeof next === "function" ? next(values[index]) : next }]
    },
    useRef(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = { current: initial }
      return values[index]
    },
    useEffect(run: () => (() => void) | void, dependencies: unknown[]) {
      const index = cursor++, previous = effects.get(index)
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) afterRender.push(() => {
        previous?.cleanup?.()
        effects.set(index, { dependencies, cleanup: run() ?? undefined })
      })
    },
  }
  const stored = new Map<string, string>()
  const browser = {
    location: { href: "https://node.example/app?conversation=chat" },
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    history: { replaceState(_state: unknown, _title: string, url: string) { browser.location.href = new URL(url, browser.location.href).href } }, setTimeout: () => 1, clearTimeout() {}, setInterval: () => 1, clearInterval() {},
  }
  const sandbox = createContext({
    window: browser, URL, TextEncoder, TextDecoder, Error, crypto: globalThis.crypto,
    setTimeout: browser.setTimeout, clearTimeout: browser.clearTimeout, setInterval: browser.setInterval, clearInterval: browser.clearInterval,
    structuredClone, navigator: { locks: { request: async (_key: string, _options: unknown, run: (lock: object) => Promise<void>) => run({}) } },
    document: { hidden: false, addEventListener() {}, removeEventListener() {} },
  })
  const signed: string[] = [], attestation = { device_id: "client", signature: "signature" }
  const botBody = (pending: unknown) => ({ ...JSON.parse(JSON.stringify(parseCreateBot(pending))), scheduler_node: "gateway", owner_attestation: attestation, attestation })
  const outboxItems = new Map<string, { key: string }>()
  const memoryOutbox = {
    all: async () => [...outboxItems.values()].map((item) => structuredClone(item)),
    put: async (item: { key: string }) => { outboxItems.set(item.key, structuredClone(item)) },
    delete: async (key: string) => { outboxItems.delete(key) },
  }
  const modules = new Map<string, { exports: Record<string, unknown> }>()
  const clientRoot = fileURLToPath(new URL("../../../../../../packages/client/src/", import.meta.url))
  function load(filename: string): Record<string, unknown> {
    const cached = modules.get(filename)
    if (cached) return cached.exports
    const module = { exports: {} }
    modules.set(filename, module)
    // Export the real hook only inside this test; production keeps its provider boundary.
    const source = readFileSync(filename, "utf8") + (filename.endsWith("/workspace/context.tsx") ? "\nexport { useWorkspaceState }\n" : "")
    const compiled = transpileModule(source, { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
    const require = (specifier: string) => {
      if (specifier === "react") return react
      if (specifier.endsWith("/lib/node.ts")) return { ...nodeSettings, messageOf: (cause: unknown) => cause instanceof Error ? cause.message : String(cause) }
      if (specifier === "react/jsx-runtime") return { jsx: () => null }
      return load(resolve(dirname(filename), specifier))
    }
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(module.exports, require, module)
    return module.exports
  }
  type Workspace = ReturnType<typeof import("@vibeface/client/workspace/context.tsx").useWorkspace>
  const workspaceModule = load(resolve(clientRoot, "workspace/context.tsx"))
  assert.equal(typeof workspaceModule.useWorkspaceState, "function")
  const { useWorkspaceState } = workspaceModule as { useWorkspaceState(platform: unknown, connection: ReadyConnection): Workspace }
  const platform = {
    secure: { attestationPayload: (network: string, scope: string, body: string) => `${network}|${scope}|${body}` },
    transport: { signClient: async (domain: string, payload: string) => { signed.push(`${domain}|${payload}`); return "signature" } },
    storage: browser.localStorage, outbox: memoryOutbox, self: "这个浏览器",
    notifier: { name: "浏览器提醒", permission: async () => "default", request: async () => "default", show() {} },
    onForeground: () => () => {}, foreground: () => true,
  }
  const bot = { bot_id: "bot", conversation_id: "chat", name: "Bot", instructions: "处理项目", models: {}, skills: null, avatar: null, avatar_version: null, event_hash: "bot-event", last_message: null, busy: null }
  const work: WorkRecord = { work_id: "work", trigger: "t", target_node: "gateway", cwd: "/project", title: "工作", prompt: "工作", state: "running", resolved: { provider: "codex", model: null }, summary: "", outcome: null, result_text: "", failure_reason: null }
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
  const acknowledgements: string[] = []
  let unreadNotifications: { notification_id: string; kind: string; text: string; conversation_id: string }[] = []
  const messages: Record<string, unknown>[] = [], controls: Record<string, unknown>[] = [], creations: Record<string, unknown>[] = []
  let failControl = false, failBotCheck = false, failMessage = false
  const questionRow = { question_id: "q1", conversation_id: "chat" as string | null, bot: { bot_id: "bot", name: "Bot" } as { bot_id: string; name: string } | null, severity: "attention", text: "要不要继续", choices: [{ label: "继续" }, { label: "停下" }], about: { node: "worker", agent: "codex", session_id: "thread", cwd: "/project", summary: "改登录" }, author: "gateway", created_at_ms: 1, answer: null }
  const otherRow = { ...questionRow, question_id: "s1", conversation_id: "other" }
  let questionRows: Record<string, unknown>[] = [questionRow, otherRow]
  const answers: Record<string, unknown>[] = []
  let answerOutcome: "answered" | "already_answered" | "lost" = "lost"
  let createMode: "unknown" | "rejected" | "success" = "unknown"
  let knownBots: ReturnType<typeof parseBot>[] = [bot]
  let knownNodes: ReturnType<typeof parseNodes> = []
  const connection: ReadyConnection = {
    generation: 1, entry: { kind: "gateway", device_id: "gateway", address: "https://node.example" },
    principal: { network_id: "network", device_id: "client", epoch: 1, can_manage: true, source: "membership" },
    info: { device_id: "gateway", name: "gateway", version: "test", initialized: true, network_id: "network" },
    isCurrent: () => activeConnection === connection, subscribe: () => () => {}, request: async (method: string) => { if (method === "write_state") return { state: "writable", conflicts: [] }; throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: knownNodes }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: unreadNotifications }
      if (method === "questions") return { questions: questionRows }
      if (method === "open_works") return { works: [] }
      if (method === "workspace_overview") return {}
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
    cursor = 0
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
    failMessage = true
    await assert.rejects(state.sendMessage("work"), /connection lost/)
    state = render()
    assert.ok(state.messagePending)
    const pending = JSON.parse(JSON.stringify(state.messagePending))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, text: "work", report_results: true, attestation })
    assert.equal(signed.at(-1), `attestation|network|chat|${JSON.stringify({ type: "message_posted", message_id: pending.message_id, role: "user", text: "work", report_results: true })}`)
    await assert.rejects(render().sendMessage("different work"), /上一条消息/)
    failMessage = false
    connection.entry.device_id = "other-gateway"
    await render().sendMessage("work")
    assert.deepEqual(messages[1], messages[0], "another entrance receives the same signed request")
    connection.entry.device_id = "gateway"
    assert.equal(render().messagePending, null)
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
    assert.equal(messages.length, 2)

    await t.test("new connections wait for their own complete read and retain unknown writes without replay", async () => {
      knownBots = [bot]
      await render().refresh()
      assert.equal(render().messagePending, null)
      for (const outcome of ["success", "failure"]) {
        const write = deferred(); messageHold = write
        const before = messages.length
        const sending = render().sendMessage("reconnect payload")
        const rejected = assert.rejects(sending, /连接换了|old response lost/)
        await new Promise(setImmediate)
        assert.equal(render().messageBusy, true)
        const original = JSON.parse(JSON.stringify(render().messagePending))
        const reading = deferred(); conversationHold = reading
        const ackBefore = acknowledgements.length
        unreadNotifications = [{ notification_id: `note-${activeConnection.generation}`, kind: "result", text: "unread", conversation_id: "chat" }]
        const next: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === next }
        activeConnection = next
        assert.equal(render().writable, false)
        await new Promise(setImmediate)
        assert.equal(render().messageBusy, false)
        assert.equal(render().writable, false)
        await assert.rejects(render().sendMessage("reconnect payload"), /补读/)
        assert.equal(messages.length, before + 1)
        if (outcome === "success") reading.resolve({ ...context, scheduler: null, works: [work] })
        else reading.reject(new Error("new read unavailable"))
        await new Promise(setImmediate)
        assert.equal(render().writable, outcome === "success")
        assert.equal(acknowledgements.length, ackBefore)
        if (outcome === "failure") await assert.rejects(render().sendMessage("reconnect payload"), /补读/)
        conversationHold = null; scheduler = null
        await render().run(() => render().refresh())
        assert.equal(render().writable, true)
        assert.deepEqual(JSON.parse(JSON.stringify(render().messagePending)), original)
        if (outcome === "success") write.resolve({ accepted: true, state: "queued", message_id: original.message_id })
        else write.reject(new Error("old response lost"))
        await rejected
        assert.equal(render().messageBusy, false)
        assert.equal(render().error, null)
        if (outcome === "success") {
          await new Promise(setImmediate)
          assert.equal(messages.length, before + 2, "the new entry's first complete read resubmits the unconfirmed message once")
          assert.deepEqual(messages.at(-1), messages.at(-2), "byte for byte, without a new signature")
          assert.equal(render().messagePending, null)
        } else {
          assert.deepEqual(JSON.parse(JSON.stringify(render().messagePending)), original)
          assert.equal(messages.length, before + 1, "a failed read resubmits nothing")
        }
      }
      messageHold = null
      const original = render().messagePending
      assert.ok(original)
      transcript = [...transcript, posted(original.message_id)]
      await render().refresh()
      assert.equal(render().messagePending, null)
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
      unreadNotifications = [{ notification_id: "after-reconnect", kind: "result", text: "unread", conversation_id: currentId }]
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
      const bridge = componentHarness('../../../../../../apps/mobile/src/data/live.tsx', {
        '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/identity/access.ts': { accessOf },
        '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/lib/node.ts': { entryAddress },
        '@vibeface/client/chat/protocol.ts': { botReach, composerHint }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, unreadCounts },
        '@vibeface/client/workspace/bot-settings.ts': { botCopy }, '@vibeface/client/workspace/inbox.ts': { inboxItems }, '@vibeface/client/workspace/labels.ts': taskLabels,
        '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers }, '@vibeface/client/workspace/storage.ts': { storageOf }, '@vibeface/client/workspace/types.ts': { isSessionNotification: () => false },
        '@/platform': { mobilePlatform: { storage: browser.localStorage } }, './context': { WorkspaceProvider: 'NativeWorkspace' },
      }, {}, 'Live')
      type NativeWorkspace = import('../../../../../../apps/mobile/src/data/types.ts').Workspace
      let native: NativeWorkspace
      const mobile = () => {
        shared = render()
        const value = bridge.render('Live').find(({ type }) => type === 'NativeWorkspace')?.props.value
        assert.ok(value && typeof value === 'object')
        native = value as NativeWorkspace
        return native
      }
      mobile()
      const chat = componentHarness('../../../../../../apps/mobile/app/b/[botId].tsx', {
        'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: botA.bot_id }) },
        'expo-router/react-navigation': { useHeaderHeight: () => 0 },
        'react-native': { Platform: { OS: 'android' }, FlatList: 'FlatList', View: 'View', StyleSheet: { absoluteFill: {} } },
        'react-native-keyboard-controller': { useReanimatedKeyboardAnimation: () => ({ height: { value: 0 } }) },
        'react-native-reanimated': { default: { View: 'AnimatedView' }, useAnimatedStyle: (read: () => unknown) => read() },
        'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }), useSafeAreaFrame: () => ({ width: 390 }) },
        'uniwind': { withUniwind: (component: unknown) => component, useUniwind: () => ({ theme: 'light' }) },
        '@/data/context': { useWorkspace: () => native }, '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
        '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/bot-settings.ts': { botCopy }, '@vibeface/client/workspace/home.ts': { unreachable },
        '@vibeface/client/chat/protocol.ts': { awaitsDevice: () => false, botReach, botCanSend, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { messageTimes: () => new Map() },
        '@/components/bot-face': { BotFace: 'BotFace', ScreenFaces: 'ScreenFaces', botMood: () => 'idle' }, '@/components/kit': { botTint: () => ({}), EmptyState: 'EmptyState' },
        '@/features/chat/work': { useChatExtras: () => ({ first: [], after: new Map(), rest: [], cards: [] }) }, '@/lib/layout': { useWide: () => false },
      }, { requestAnimationFrame: (run: () => void) => run(), setInterval: () => 1, clearInterval() {} })
      const stack = { Screen: 'Screen', Protected: 'Protected' }
      const layout = componentHarness('../../../../../../apps/mobile/app/_layout.tsx', {
        'expo-router': { Stack: stack }, 'expo-linking': { useLinkingURL: () => null }, '@/data/live': { LiveWorkspace: 'LiveWorkspace', useLive: () => shared },
        '@/data/context': { useWorkspace: () => ({ session: { phase: 'ready' } }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
        'uniwind': { useCSSVariable: () => ['white', 'black'] }, '@/lib/layout': { useWide: () => false }, 'react-native': { Platform: { OS: 'ios' }, View: 'View' },
      })
      try {
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
          { notification_id: 'native-a', kind: 'result', text: 'A 的通知', conversation_id: 'chat' },
          { notification_id: 'native-b', kind: 'result', text: 'B 的通知', conversation_id: 'chat-b' },
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
      } finally { chat.close(); layout.close(); bridge.close() }
    })
  } finally {
    for (const effect of effects.values()) effect.cleanup?.()
  }
})

function componentHarness(path: string, dependencies: Record<string, unknown>, globals: Record<string, unknown> = {}, privateExport = '') {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const focusEffects = new Map<number, { run: () => (() => void) | void; cleanup?: () => void }>()
  type Props = Record<string, unknown> & { name?: string; type?: string; role?: string; disabled?: boolean; value?: unknown; onChange?: (event: unknown) => void; onSubmit?: (event?: unknown) => void; onClick?: () => void; action?: () => Promise<unknown> }
  const elements: { type: unknown; props: Props }[] = [], afterRender: (() => void)[] = []
  let cursor = 0, focused = true
  function effect(run: () => (() => void) | void, dependencies: unknown[]) {
    const index = cursor++, previous = effects.get(index)
    if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) afterRender.push(() => {
      previous?.cleanup?.(); effects.set(index, { dependencies, cleanup: run() ?? undefined })
    })
  }
  const react = {
    createContext: () => ({}), useContext: () => null,
    useId: () => 'component',
    useSyncExternalStore: (_subscribe: unknown, snapshot: () => unknown) => snapshot(),
    useState(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = typeof initial === 'function' ? initial() : initial
      return [values[index], (next: unknown) => { values[index] = typeof next === 'function' ? next(values[index]) : next }]
    },
    useRef(initial: unknown) { const index = cursor++; if (!(index in values)) values[index] = { current: initial }; return values[index] },
    useCallback(callback: unknown, dependencies: unknown[]) {
      const index = cursor++
      const previous = values[index] as { callback: unknown; dependencies: unknown[] } | undefined
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) values[index] = { callback, dependencies }
      return (values[index] as { callback: unknown }).callback
    },
    useMemo(read: () => unknown, dependencies: unknown[]) {
      const index = cursor++, previous = values[index] as { value: unknown; dependencies: unknown[] } | undefined
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) values[index] = { value: read(), dependencies }
      return (values[index] as { value: unknown }).value
    },
    useEffect: effect, useLayoutEffect: effect,
  }
  const jsx = (type: unknown, props: Props) => { const element = { type, props }; elements.push(element); return element }
  const ui = new Proxy({}, { get: (_target, name) => name === 'useAction' ? () => ({ busy: null, error: null, run() {}, clear() {} }) : String(name) })
  const module: { exports: Record<string, unknown> } = { exports: {} }
  const useFocusEffect = (run: () => (() => void) | void) => {
    const index = cursor++, previous = focusEffects.get(index)
    if (!previous || previous.run !== run) afterRender.push(() => {
      previous?.cleanup?.()
      focusEffects.set(index, { run, cleanup: focused ? run() ?? undefined : undefined })
    })
  }
  const require = (specifier: string): unknown => {
    if (specifier === '@/features/chat/memory-summary') {
      const filename = fileURLToPath(new URL('../../../../../../apps/mobile/src/features/chat/memory-summary.tsx', import.meta.url))
      const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
      const memoryModule = { exports: {} }
      new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext({ Error, TextEncoder, ...globals })(memoryModule.exports, require, memoryModule)
      return memoryModule.exports
    }
    return specifier === 'react' ? react : specifier === 'react/jsx-runtime' ? { jsx, jsxs: jsx } : specifier === 'expo-router' ? { Link: 'Link', useFocusEffect, ...dependencies[specifier] as object } : dependencies[specifier] ?? ui
  }
  const filename = fileURLToPath(new URL(path, import.meta.url))
  const source = readFileSync(filename, 'utf8') + (privateExport ? `\nexport { ${privateExport} }\n` : '')
  const compiled = transpileModule(source, { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
  new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext({ Map, Error, TextEncoder, ...globals })(module.exports, require, module)
  return {
    render(name: string, props: unknown = {}) {
      cursor = 0; elements.length = 0
      const component = module.exports[name]
      assert.ok(typeof component === 'function')
      component(props)
      afterRender.splice(0).forEach(run => run())
      return elements
    },
    focus() { if (!focused) { focused = true; for (const effect of focusEffects.values()) effect.cleanup = effect.run() ?? undefined } },
    blur() { if (focused) { focused = false; for (const effect of focusEffects.values()) { effect.cleanup?.(); effect.cleanup = undefined } } },
    close() { for (const effect of [...effects.values(), ...focusEffects.values()]) effect.cleanup?.() },
    module: module.exports,
  }
}

const pendingResult = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settleComponent = () => new Promise(setImmediate)

test("provider cards keep summary readiness, setup and execution details in their own roles", async (t) => {
  const ready: ProviderReadiness = { state: 'ready' }
  const signIn: ProviderReadiness = { state: 'needs_setup', issue: { stage: 'authentication', code: 'sign_in_required' } }
  const model: ProviderReadiness = { state: 'unverified', issue: { stage: 'model_list', code: 'model_unverified' } }
  const signInText = 'Codex 需要登录，请在运行设备完成登录'
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
      provider: 'codex', roles: example.roles, supports: ['agent', 'execution'], bin: '/usr/bin/codex', app_server: null,
      execution_model: 'saved-model', execution_readiness: example.execution_readiness,
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
    const app = componentHarness(platform === 'web' ? '../devices/ai-settings.tsx' : '../../../../../../apps/mobile/app/ai.tsx', {
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

test("mobile provider probes discard settings that arrive after another call or connection", async () => {
  for (const changed of ['call', 'connection', 'none']) {
    const held = pendingResult()
    let current = true, reads = 0, snapshot: unknown
    const connection = { isCurrent: () => current, request: async (method: string) => {
      if (method === 'node_settings') return ++reads === 1 ? 'initial settings' : held.promise
      return method === 'configure_node' ? 'new settings' : 'probe result'
    } }
    const app = componentHarness('../../../../../../apps/mobile/src/features/devices/remote-settings.tsx', {
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

const parityBot = parseBot({ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot', instructions: '', models: {}, skills: null, avatar: null, avatar_version: null, event_hash: 'bot-event', last_message: null, busy: null })
const parityNode = { device_id: 'worker', name: '工作设备', online: true, local: false, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready' as const, relay: false, roots: [], codex_connected: true } }
const parityWork = (extra: Partial<WorkRecord> = {}) => {
  const work = parseConversation({ ...context, works: [{ resolved: { provider: 'codex', model: null }, work_id: 'work', trigger: 'message:one', target_node: 'worker', cwd: '/project', title: '核对项目', prompt: '完整请求', state: 'running', summary: '✓ 读记录\n→ 核对结果\n· 等待同步', outcome: null, result_text: '', failure_reason: null, kind: 'codex', thread_id: 'session', ...extra }] }).works[0]
  assert.ok(work && work.resolved)
  return work
}
const paritySnapshot = (value: unknown) => value === null ? null : parseConversation(value)
const parityShared = () => ({
  generation: 1, isCurrent: () => true, synced: true, connectionOnline: true, conversationId: 'chat', data: paritySnapshot(context),
  bots: [parityBot], selectedBot: parityBot, nodes: [parityNode], openWorks: [{ ...parityWork(), conversation_id: 'chat' }],
  overview: workspaceModel.parseWorkspaceOverview({}), questions: [] as ReturnType<typeof parseConversation>['questions'], answered: {} as Record<string, string>, liveProgress: [], writable: true,
  writeState: { state: 'writable' as const, conflicts: [] }, notifications: [], messagePending: null, messageBusy: false,
  setView(_view: string) {}, selectBot: async (_bot: typeof parityBot) => {}, interruptWork: async (_work: string) => {}, sendMessage: async () => true,
})

test('both task overviews preserve known records, show their coverage and only navigate cross-Bot program checks', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    shared.openWorks = Array.from({ length: 257 }, (_, index) => ({ ...parityWork({ work_id: `work-${index}`, prompt: `完整请求 ${index}` }), conversation_id: 'chat' }))
    shared.overview = workspaceModel.parseWorkspaceOverview({
      bots: [parityBot, { bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' }],
      tasks: shared.openWorks.slice(0, 256), truncated: true, consistency: 'partial', checks: [{ check_id: 'other-process', conversation_id: 'other-chat', trigger: 'message:remote' }],
      coverage: { source: 'workspace_overview', truncated: true, omitted_tasks: 1, omitted_questions: 2, remote_owners: [{ owner: 'remote', reason: 'remote_scheduler_not_queried' }] },
    })
    const dependencies = {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared, useInbox: () => [] },
      '@/data/context': { useWorkspace: () => ({ bots: shared.bots }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/lib/errors.ts': errors, './inbox': { useInbox: () => [] }, './workspace-shell': { useCreateBot: () => () => {} },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }, '@/components/kit': { providerIcons: { codex: 'CodexIcon' } },
    }
    const app = componentHarness(platform === 'web' ? './tasks-overview.tsx' : '../../../../../../apps/mobile/src/features/home/tasks.tsx', dependencies)
    const screen = platform === 'web' ? 'TasksOverview' : 'TasksScreen'
    const cardName = platform === 'web' ? 'TaskOverviewCard' : 'TaskRow'
    const mastheadName = platform === 'web' ? 'WorkspaceMasthead' : 'MobileMasthead'
    try {
      let elements = app.render(screen)
      const cards = elements.filter(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.equal(cards.length, 257, `${platform} keeps the row outside the bounded overview`)
      assert.equal((cards[256].props.work as WorkRecord).prompt, '完整请求 256')
      assert.match(componentText(elements), /总览已截断/)
      assert.match(componentText(elements), /本次总览未返回 1 项任务/)
      assert.match(componentText(elements), /本次总览未返回 2 项问题/)
      assert.doesNotMatch(componentText(elements), /另有.*未列出/)
      assert.match(componentText(elements), /尚未读取其他设备的后台工作/)
      assert.doesNotMatch(componentText(elements), /0 项.*未列出/)
      assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, 'another Bot program check cannot settle against the active conversation')
      assert.equal(nestedElements(elements, ['AttentionActivity', 'OverviewCheckCard']).some(({ type, props }) => type === 'Link' && (platform === 'web' ? (props.params as { botId?: string })?.botId === 'other' : props.href === '/b/other')), true)
      shared.nodes = [{ ...parityNode, online: false }]
      elements = app.render(screen)
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.attention, 258)
      const offlineCard = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.ok(offlineCard && typeof offlineCard.type === 'function')
      assert.match(componentText(offlineCard.type(offlineCard.props)), /待确认/)
      shared.nodes = [parityNode]
      shared.connectionOnline = false
      elements = app.render(screen)
      const staleCard = elements.find(({ type }) => typeof type === 'function' && type.name === cardName)
      assert.ok(staleCard && typeof staleCard.type === 'function')
      assert.match(componentText(staleCard.type(staleCard.props)), /状态未知/)
      shared.synced = false
      shared.openWorks = []
      shared.overview = workspaceModel.parseWorkspaceOverview({})
      elements = app.render(screen)
      assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === mastheadName)?.props.working, null)
      assert.doesNotMatch(componentText(elements), /没有未完成的任务/)
      shared.synced = true
      shared.connectionOnline = true
      elements = app.render(screen)
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
    const app = componentHarness(platform === 'web' ? './tasks-overview.tsx' : '../../../../../../apps/mobile/src/features/home/tasks.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared, useInbox: () => [] },
      '@/data/context': { useWorkspace: () => ({ bots: shared.bots }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      './inbox': { useInbox: () => [] }, './workspace-shell': { useCreateBot: () => () => {} }, '@/components/kit': { providerIcons: {} },
    })
    const render = () => app.render(platform === 'web' ? 'TasksOverview' : 'TasksScreen')
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
    const app = componentHarness(platform === 'web' ? './tasks-overview.tsx' : '../../../../../../apps/mobile/src/features/home/tasks.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared, useInbox: () => [] },
      '@/data/context': { useWorkspace: () => ({ bots: shared.bots }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/lib/errors.ts': errors, './inbox': { useInbox: () => [] }, './workspace-shell': { useCreateBot: () => () => {} },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }, '@/components/kit': { providerIcons: { codex: 'CodexIcon' } },
    })
    try {
      for (const observedFailure of [true, false]) {
        const running = { ...parityWork({ summary: '', result_text: '完整输出', failure_reason: '旧快照记录的说明' }), conversation_id: 'chat' }
        const failed = { ...parityWork({ ...running, state: 'finished', outcome: 'failed', failure_reason: '这次执行失败，已保留改动' }), conversation_id: running.conversation_id }
        shared.openWorks = [observedFailure ? running : failed]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...(observedFailure ? failed : running), kind: 'work' }] })
        const elements = app.render(platform === 'web' ? 'TasksOverview' : 'TasksScreen')
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'TaskOverviewCard' : 'TaskRow'))
        assert.ok(card && typeof card.type === 'function')
        const text = componentText(card.type(card.props))
        assert.match(text, /失败/, `${platform} uses the outcome rather than treating finished as success`)
        assert.match(text, /这次执行失败，已保留改动/)
        assert.doesNotMatch(text, /旧快照记录的说明/)
        assert.doesNotMatch(text, /已完成/)
        assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'WorkspaceMasthead' : 'MobileMasthead'))?.props.attention, 1)
        assert.equal((card.props.work as WorkRecord).result_text, '完整输出')
      }
      for (const [known, observed] of [['completed', 'unverified'], ['unverified', 'completed'], ['completed', 'failed'], ['failed', 'completed']] as const) {
        shared.openWorks = [{ ...parityWork({ state: known === 'unverified' ? 'unverified' : 'finished', outcome: known, summary: '' }), conversation_id: 'chat' }]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...parityWork({ state: observed === 'unverified' ? 'unverified' : 'finished', outcome: observed }), conversation_id: 'chat', kind: 'work' }] })
        const elements = app.render(platform === 'web' ? 'TasksOverview' : 'TasksScreen')
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'TaskOverviewCard' : 'TaskRow'))
        assert.ok(card && typeof card.type === 'function')
        const text = componentText(card.type(card.props))
        assert.match(text, /待确认|状态未知/)
        assert.doesNotMatch(text, /已完成/)
        assert.equal(elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'WorkspaceMasthead' : 'MobileMasthead'))?.props.attention, 1)
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
    const app = componentHarness(platform === 'web' ? './tasks-overview.tsx' : '../../../../../../apps/mobile/src/features/home/tasks.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared, useInbox: inbox },
      '@/data/context': { useWorkspace: () => ({ bots: shared.bots }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/lib/errors.ts': errors, './inbox': { useInbox: inbox }, './workspace-shell': { useCreateBot: () => () => {} },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }, '@/components/kit': { providerIcons: { codex: 'CodexIcon' } },
    })
    const render = () => app.render(platform === 'web' ? 'TasksOverview' : 'TasksScreen')
    const count = (elements: ReturnType<typeof render>) => elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'WorkspaceMasthead' : 'MobileMasthead'))?.props.attention
    try {
      for (const unreachable of ['offline', 'disconnected']) {
        shared.nodes = [entry, { ...parityNode, online: unreachable !== 'offline', capabilities: { ...parityNode.capabilities, codex_connected: unreachable !== 'disconnected' } }]
        shared.openWorks = [{ ...parityWork({ summary: '' }), conversation_id: 'chat' }]
        shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], tasks: [{ ...shared.openWorks[0], kind: 'work', state: 'finished', outcome: 'failed', failure_reason: '总览记录的失败原因' }] })
        const elements = render()
        assert.equal(elements.some(({ type }) => type === 'ConfirmCard'), false, `${platform} cannot offer settlement for an already failed work`)
        const card = elements.find(({ type }) => typeof type === 'function' && type.name === (platform === 'web' ? 'TaskOverviewCard' : 'TaskRow'))
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

test('both task details bind approval to current unfinished work and keep stops above approval', () => {
  for (const platform of ['web', 'native']) {
    const shared = parityShared()
    const approval = { question_id: 'approval', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '继续吗', choices: [], author: 'worker', created_at_ms: 2, answer: null, closed: null, about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' } }
    shared.data = parseConversation({ ...context, works: [parityWork({ work_id: 'old', state: 'finished', outcome: 'completed' }), parityWork()], questions: [approval] })
    let workId = 'old'
    const app = componentHarness(platform === 'web' ? './task-detail.tsx' : '../../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      '@tanstack/react-router': { Link: 'Link', useNavigate: () => () => {} }, 'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ push() {}, back() {} }), useLocalSearchParams: () => ({ botId: 'bot', workId }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': taskLabels,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@/components/app/kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: () => Promise.resolve() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      './task-activity': { confirmText: () => '设备无法报告结果', BackOnline: 'BackOnline' },
    }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
    const render = () => app.render(platform === 'web' ? 'TaskDetail' : 'default', { botId: 'bot', workId })
    try {
      assert.equal(render().some(({ type }) => type === 'QuestionCard'), false, `${platform} old work cannot inherit the later approval`)
      workId = 'work'
      let elements = render()
      assert.equal(elements.some(({ type }) => type === 'QuestionCard'), true)
      assert.match(componentText(elements), /需要你同意/)
      assert.equal(elements.some(({ props }) => props.label === '最近步骤摘要 · 3'), true)
      shared.answered = { approval: '继续' }
      assert.equal(render().some(({ type }) => type === 'QuestionCard'), false)
      shared.answered = {}
      shared.data = parseConversation({ ...context, works: [parityWork()], deliveries: [{ work_id: 'work', target_node: 'worker', state: 'stop_waiting' }], questions: [approval] })
      elements = render()
      assert.match(componentText(elements), /停止中/)
      assert.doesNotMatch(componentText(elements), /需要你同意/)
      shared.nodes = [{ ...parityNode, capabilities: { ...parityNode.capabilities, codex_connected: false } }]
      assert.match(componentText(render()), /待确认/)
      shared.nodes = [parityNode]
      shared.data = parseConversation({ ...context, works: [parityWork({ kind: 'task', coordination_job: { trigger: 'task:work', state: 'queued', lane: 'task', hold_reason: 'machine_slot' } })] })
      elements = render()
      assert.equal(elements.some(({ props }) => props.label === '协调回合'), true)
      assert.match(componentText(elements), /等待设备运行名额/)
      assert.equal(shared.data.works[0].state, 'running', 'queue observations leave DAG work state intact')
    } finally { app.close() }
  }
})

test('context shows scheduler observations without promising native resume or repeating external sessions', () => {
  const shared = parityShared()
  const data = parseConversation({ ...context, scheduler: { jobs: [{ trigger: 'message:queued', state: 'queued' }], unanswered: [] } })
  const dependencies = { '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/chat/protocol.ts': { runtimeLabel }, '@vibeface/client/workspace/bot-settings.ts': { scheduleFrequency: () => '一次', formatWhen: () => '刚才' }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } }
  const app = componentHarness('./context-panel.tsx', dependencies)
  try {
    const elements = app.render('ContextPanel', { bot: parityBot, data, nodes: shared.nodes, connected: true, onBack() {} })
    assert.equal(elements.filter(({ type }) => type === 'CollaborationPanel').length, 1)
    const continuity = elements.find(({ type }) => typeof type === 'function' && type.name === 'AgentContinuity')
    assert.ok(continuity && typeof continuity.type === 'function')
    let tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /排队中/)
    assert.doesNotMatch(componentText(tree), /正在运行|首次模型|续接记录|设备默认模型 · 设备默认模型/)
    assert.equal(elements.filter(({ type }) => typeof type === 'function' && type.name === 'ContinuitySession').length, 0)
    data.scheduler!.jobs = [{ trigger: 'message:stop', state: 'cancelling' }]
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /停止中/)
    data.scheduler!.jobs = [{ trigger: 'message:publish', state: 'publishing' }]
    assert.match(componentText(continuity.type(continuity.props)), /回复同步中/)
    data.scheduler!.provider = 'claude'
    data.scheduler!.model = 'new-device-default'
    data.scheduler!.jobs = [{ trigger: 'message:read-back', state: 'reconciling', runtime: workspaceModel.parseTurnRuntime({ provider: 'codex', model: 'frozen-model', reasoning_effort: 'high', generation: 'config', session_generation: 1, delivery_id: 'delivery', stage: 'read_back', execution: 'unknown', attempted_at_ms: 1, switched_from: null, switch_reason: null }) }]
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /正在查询上次回复/)
    assert.match(componentText(tree), /Codex · 提交模型 frozen-model/)
    assert.doesNotMatch(componentText(tree), /new-device-default|Claude Code/)
    data.scheduler = parseConversation({ ...context, scheduler: { provider: 'codex', model: 'current-model', unanswered: [], jobs: [{ trigger: 'message:reconcile', state: 'reconciling', runtime: { provider: 'claude', model: 'frozen-model', reasoning_effort: 'high', generation: 'generation', session_generation: 1, delivery_id: 'delivery', stage: 'read_back', execution: 'unknown', attempted_at_ms: 1, switched_from: null, switch_reason: null } }] } }).scheduler
    tree = continuity.type(continuity.props)
    assert.match(componentText(tree), /正在查询上次回复/)
    assert.match(componentText(tree), /Claude Code.*frozen-model/)
    assert.doesNotMatch(componentText(tree), /current-model/)
    const runtime = data.scheduler?.jobs[0]?.runtime
    assert.ok(runtime)
    for (const state of ['publishing', 'pending_delivery']) {
      data.scheduler = parseConversation({ ...context, scheduler: { unanswered: [], jobs: [{ trigger: 'message:history', state: 'completed', runtime: { ...runtime, model: 'newer-history', attempted_at_ms: 2 } }, { trigger: 'message:active', state, runtime }] } }).scheduler
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
    scheduler: { jobs: [{ trigger: 'message:q', state: 'queued' }, { trigger: 'message:p', state: 'publishing' }, { trigger: 'message:u', state: 'future' }, { trigger: 'message:d', state: 'completed' }], unanswered: [] },
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
    const app = componentHarness(route === 'context' ? '../../../../../../apps/mobile/app/b/[botId]/context.tsx' : '../../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot', workId: 'work' }), useRouter: () => ({ push() {}, back() {} }) },
      '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/lib/errors.ts': errors, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@/components/kit': { EmptyState: 'EmptyState', Details: 'Details', providerIcons: { codex: 'CodexIcon' }, toneText: {} },
    })
    const emptyTitle = (elements: ReturnType<typeof app.render>) => elements.find(({ type }) => type === 'EmptyState')?.props.title
    try {
      assert.match(String(emptyTitle(app.render('default'))), /正在读取/)
      assert.deepEqual(views, ['other'], 'read-only routes never acknowledge chat notifications')
      generation = 2
      shared = { ...shared, generation, isCurrent: () => generation === 2, selectBot: () => next.promise.then(() => {}) }
      app.render('default')
      next.reject(new Error('当前读取失败'))
      await settleComponent()
      assert.match(String(emptyTitle(app.render('default'))), /当前读取失败/)
      old.resolve(undefined)
      await settleComponent()
      assert.match(String(emptyTitle(app.render('default'))), /当前读取失败/, 'an old successful select cannot erase this connection error')
      const retry = app.render('default').find(({ type, props }) => type === 'Button' && typeof props.onPress === 'function')
      assert.ok(retry && typeof retry.props.onPress === 'function')
      shared.selectBot = async () => {}
      retry.props.onPress()
      app.render('default')
      await settleComponent()
      shared.conversationId = 'chat'
      let elements = app.render('default')
      assert.equal(elements.some(({ type }) => typeof type === 'function' && type.name === 'ContextRecords'), false, 'matching selection alone cannot expose a foreign payload')
      assert.doesNotMatch(componentText(elements), /秘密记录/)
      shared.data = parseConversation({ ...context, works: [parityWork()] })
      elements = app.render('default')
      assert.match(componentText(elements), /当前连接尚未完成首读/)
      shared.synced = true
      shared.bots = []
      assert.match(String(emptyTitle(app.render('default'))), /找不到这个 Bot/)
      shared.bots = [parityBot]
      shared.data = parseConversation(context)
      elements = app.render('default')
      if (route === 'work') assert.match(String(emptyTitle(elements)), /找不到这个任务/)
      else assert.equal(elements.some(({ type }) => typeof type === 'function' && type.name === 'ContextRecords'), true)
      shared.connectionOnline = false
      elements = app.render('default')
      assert.match(componentText(elements) + String(emptyTitle(elements)), /状态未知/)
    } finally { app.close() }
  }
})

test('native context renders shared memory, external attribution, scheduler states and honest sent records', () => {
  const shared = parityShared()
  shared.data = parseConversation({ ...context,
    bot_settings: { skills: [], memory: [{ memory_id: 'm', text: '共享笔记', metadata: { epoch: 12, revision: 'a'.repeat(64), control_revision: 0, source_event_hash: 'b'.repeat(64), origin: 'agent', user_controlled: false, essential: true } }], memory_sync_conflicts: { items: [], has_more: false }, schedules: [] },
    scheduler: { jobs: [{ trigger: 'message:p', state: 'publishing' }, { trigger: 'message:d', state: 'completed' }], unanswered: [] },
    sessions: [{ node: 'worker', agent: 'codex', session_id: 'external', cwd: '/shared', summary: '外部摘要', muted: true, created_at_ms: 1 }],
    sent: [{ message_id: 'sent', to: 'chat', text: '协作请求', created_at_ms: 2 }],
  })
  shared.nodes = [{ ...parityNode, online: false }]
  const app = componentHarness('../../../../../../apps/mobile/app/b/[botId]/context.tsx', {
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
      stable_fingerprint: 'a'.repeat(64), dynamic_fingerprint: 'b'.repeat(64), stable_bytes: 100, dynamic_bytes: 200,
      invalidation_reason: 'stable_prompt_or_provider_changed', session_status: { resumable: true, pending_generation: true },
      coverage: { messages_omitted: 1, works_omitted: 0, dispatches_omitted: 0, questions_omitted: 0, attachments_omitted: 0, memory_omitted: 3, checkpoint_omitted: 1, handoff_omitted: 0, pending_checks_omitted: 2, bot_indexes_omitted: { skills: 4 }, history_retrieval: 'context' },
      native_token: 'private-native-token', rendered: 'private-prompt',
    }
    shared.data = parseConversation({ ...context, scheduler: { jobs: [], unanswered: [], context: projection }, bot_settings: {
      skills: [], schedules: [], memory_staged_count: 2, memory_sync_conflicts: { items: [], has_more: false },
      memory_conflicts: [{ call_ref: 'remember', memory_id: 'memory', expected_epoch: 9, actual_epoch: 9, expected_revision: 'old', actual_revision: 'new', reason: 'revision_changed' }],
      memory: [{ memory_id: 'memory', text: '已保存的事实', metadata: { epoch: 12, revision: 'a'.repeat(64), control_revision: 1, origin: 'user', user_controlled: true, essential: true, source_event_hash: 'b'.repeat(64) } }],
    } })
    const dependencies = {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) }, '@/data/live': { useLive: () => shared },
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/bot-settings.ts': { scheduleFrequency: () => '一次', formatWhen: () => '刚才' },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }, '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, SettingsGroup: 'SettingsGroup', SettingsRow: 'SettingsRow', Details: 'Details' },
    }
    const app = componentHarness(platform === 'web' ? './context-panel.tsx' : '../../../../../../apps/mobile/app/b/[botId]/context.tsx', dependencies)
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
      shared.data!.scheduler!.remote = { owner: 'owner', reachable: false }
      assert.match(JSON.stringify(render()), /当前状态未知/)
    } finally { app.close() }
  }
})

test('native Stack focus restores non-chat views while the chat route owns its activation', () => {
  const views: string[] = []
  const stack = { Screen: 'Screen', Protected: 'Protected' }
  const app = componentHarness('../../../../../../apps/mobile/app/_layout.tsx', {
    'expo-router': { Stack: stack }, 'expo-linking': { useLinkingURL: () => null },
    '@/data/live': { LiveWorkspace: 'LiveWorkspace', useLive: () => ({ setView: (view: string) => { views.push(view) } }) },
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
  const app = componentHarness("../../../../../../apps/mobile/src/features/home/tasks.tsx", {
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
  const app = componentHarness("../../../../../../apps/mobile/src/features/home/tasks.tsx", {
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

test("question cards select locally, send exact answers only on confirmation, and keep unknown retries and winning answers", async () => {
  const sent: unknown[][] = [], notices: string[] = []
  let actionBusy: string | null = null, answerStatus = 'answered'
  const workspace = { generation: 1, writable: true, nodes: [], answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], announce: (notice: string) => { notices.push(notice) },
    bots: [{ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot' }], openWorks: [], data: { works: [{ work_id: 'w', thread_id: 's', title: '升级依赖', outcome: 'completed' }] },
    answer: async (...args: unknown[]) => { sent.push(args); return { status: answerStatus, text: answerStatus === 'answered' ? args[1] : '停下' } } }
  const toasts: { text: string; action?: { label: string; onClick(): void } }[] = [], visited: unknown[] = []
  const kit = new Proxy({ useAction: () => ({ busy: actionBusy, error: null, run: (_key: string, work: () => Promise<void>) => work() }), toast: (text: string, options: { action?: { label: string; onClick(): void } } = {}) => { toasts.push({ text, action: options.action }) } }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const router = { Link: 'Link', useNavigate: () => (target: unknown) => { visited.push(target) } }
  const app = componentHarness('./question-card.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'web', source: 'membership', can_manage: false } } }) }, '@vibeface/client/workspace/labels.ts': { nodeName: () => '工作电脑' }, '@vibeface/client/workspace/questions.ts': { agentName, answerLabel, attentionLabel, approvalTask, declined, questionMeta, questionParts, settledAnswer, sharedProject }, '@/components/app/kit': kit, '@tanstack/react-router': router })
  const longChoice = '只在本机验证完整配置并保留现有服务，检查通过后再决定是否发布'
  const question = { question_id: 'q1', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '\n是否允许继续？\n已完成本机检查。\n\n线上结果未知。', choices: [{ label: '允许' }, { label: longChoice }, { label: '停下' }], about: { node: 'worker', agent: 'codex', session_id: 's', cwd: '/path/to/web/', summary: '改登录' }, author: 'gateway', created_at_ms: 1, answer: null, closed: null }
  const render = (props: object = {}) => app.render('QuestionCard', { question, ...props })
  const choose = (value: string) => {
    const option = render().find(({ type, props }) => type === 'input' && props.value === value)
    assert.ok(option?.props.onChange, `missing radio ${value}`)
    option.props.onChange({})
  }
  const send = async (props: object = {}) => {
    const button = render(props).find(({ type, props }) => type === 'Button' && componentText(props.children).includes('发送回答'))
    assert.ok(button?.props.onClick)
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
    assert.equal(elements.filter(({ type, props }) => type === 'input' && props.type === 'radio').length, 4, 'three real choices and one custom mode')
    assert.equal(elements.filter(({ type }) => type === 'input').every(({ props }) => props.checked === false), true, 'no answer is recommended or selected by default')
    assert.equal(elements.some(({ type }) => type === 'textarea' || type === 'form'), false)
    assert.equal(elements.some(({ props }) => props.onKeyDown || props['aria-keyshortcuts']), false, 'digits have no answer shortcut')
    assert.ok(elements.some(({ props }) => props.text === '已完成本机检查。\n\n线上结果未知。'), 'the decision context keeps its paragraphs')
    await send()
    assert.equal(sent.length, 0)
    assert.match(componentText(render()), /选一个回答，或自己写回答。/)
    choose('允许')
    assert.equal(sent.length, 0, 'selecting approval never sends it')
    assert.equal(render().find(({ type, props }) => type === 'input' && props.value === '允许')?.props.checked, true)
    await send()
    assert.deepEqual(plain(sent.at(-1)), [question, '允许'], 'the approval label remains exact')
    choose(longChoice)
    assert.equal(sent.length, 1)
    await send()
    assert.deepEqual(plain(sent.at(-1)), [question, longChoice], 'the complete choice is submitted')
    choose('custom')
    for (const value of ['1', '１', '先检查配置\n确认后再继续']) {
      write(value)
      const input = render().find(({ type }) => type === 'textarea')
      assert.ok(input?.props.id)
      assert.equal(render().some(({ type, props }) => type === 'label' && props.htmlFor === input.props.id && componentText(props.children) === '你的回答'), true)
      assert.equal(input.props.onKeyDown, undefined, 'Enter and IME stay native textarea input')
      await send()
      assert.deepEqual(plain(sent.at(-1)), [question, value], 'custom text is literal, including digits and newlines')
    }
    const count = sent.length
    choose('停下'); choose('custom')
    assert.equal(render().find(({ type }) => type === 'textarea')?.props.value, '先检查配置\n确认后再继续', 'switching modes keeps the handwritten draft')
    write('   '); await send()
    assert.equal(sent.length, count, 'whitespace cannot be submitted')
    elements = render()
    const invalid = elements.find(({ type }) => type === 'textarea')
    assert.equal(invalid?.props['aria-invalid'], true)
    assert.ok(elements.some(({ props }) => props.role === 'alert' && String(invalid?.props['aria-describedby']).includes(String(props.id))))
    write('新回答')
    workspace.writable = false
    elements = render()
    assert.equal(elements.find(({ type }) => type === 'fieldset')?.props.disabled, true)
    assert.equal(elements.find(({ type }) => type === 'textarea')?.props.disabled, true)
    await send()
    assert.equal(sent.length, count, 'the submit handler also enforces read-only state')
    workspace.writable = true
    workspace.pendingAnswers = [{ question_id: 'q1', text: '继续\n原文' }]
    elements = render()
    assert.equal(elements.some(({ type }) => type === 'input' || type === 'textarea'), false, 'a pending answer cannot be replaced')
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
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'input'), false)
    workspace.pendingAnswers = []
    elements = render()
    assert.match(componentText(elements), /已回答 · 允许/, 'a shared answer folds the card immediately')
    assert.doesNotMatch(componentText(elements), /升级依赖/, 'an ordinary question names no held task')
    elements = render({ question: { ...question, about: { ...question.about, request_id: 'r' } } })
    assert.match(componentText(elements), /升级依赖[^]*工作电脑/)
    workspace.answered = {}
    elements = render({ question: { ...question, closed: 'request_lost' } })
    assert.match(componentText(elements), /已失效/)
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'input'), false)
    const four = { ...question, choices: [...question.choices, { label: '先核对发布目标' }] }
    assert.equal(render({ question: four }).filter(({ type, props }) => type === 'input' && props.type === 'radio').length, 5, 'four alternatives remain complete')
    choose('允许')
    await send({ inbox: true })
    assert.deepEqual(toasts.map(({ text }) => text), ['已回答'])
    toasts[0].action?.onClick()
    assert.deepEqual(plain(visited), [{ to: '/b/$botId', params: { botId: 'bot' } }])
    answerStatus = 'already_answered'
    await send({ inbox: true })
    assert.deepEqual(notices, ['这个问题已经回答过了'], 'the existing race announcement is retained')
    assert.equal(toasts.length, 1, 'a losing answer has no success toast')
  } finally { app.close() }
})

test("a question without choices opens a labelled multiline answer directly", () => {
  const app = componentHarness('./question-card.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ writable: true, nodes: [], answered: {}, pendingAnswers: [], bots: [], openWorks: [] }) },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备' },
    '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, questionMeta, questionParts, settledAnswer, sharedProject },
    '@/components/app/kit': { useAction: () => ({ busy: null, error: null }) },
    '@tanstack/react-router': { useNavigate: () => () => {} },
  })
  try {
    const elements = app.render('QuestionCard', { question: { question_id: 'free', text: '需要什么配置？', choices: [], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: null, closed: null } })
    assert.equal(elements.find(({ type }) => type === 'textarea')?.props.rows, 3)
    assert.equal(elements.some(({ type }) => type === 'fieldset'), false)
  } finally { app.close() }
})

test("mobile questions select before sending, preserve literal text and retry only the unknown answer", async () => {
  for (const value of ['允许', '1', '１', '先核对配置\n确认后再继续']) {
    const sent: unknown[][] = []
    let hold = pendingResult()
    const shared = { writable: true, answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], openWorks: [], nodes: [] }
    const question = { question_id: 'mobile', text: '\n是否允许继续？\n线上结果未知。', choices: [{ label: '允许' }, { label: '先核对配置' }, { label: '停下' }, { label: '保持现状' }], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: null, closed: null }
    const app = componentHarness('../../../../../../apps/mobile/src/features/chat/question-card.tsx', {
      '@/data/context': { useWorkspace: () => ({ members: [], bots: [], actions: { answer: (...args: unknown[]) => { sent.push(args); return hold.promise } } }) },
      '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'mobile', source: 'membership', can_manage: false } } }) },
      '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, questionMeta, questionParts, settledAnswer, sharedProject },
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
      assert.equal(render().filter(({ props }) => props.accessibilityRole === 'radio').length, 5)
      press('发送回答')
      assert.equal(sent.length, 0)
      if (value === '允许') press('允许')
      else {
        press('自己写回答')
        const input = render().find(({ type }) => type === 'TextInput')
        assert.equal(input?.props.multiline, true)
        assert.equal(input?.props.submitBehavior, 'newline')
        assert.equal(input?.props.onSubmitEditing, undefined)
        const write = input?.props.onChangeText
        assert.ok(typeof write === 'function')
        write(value)
      }
      assert.equal(sent.length, 0, 'selecting or writing is local')
      shared.writable = false
      press('发送回答')
      assert.equal(sent.length, 0, 'read-only state also guards the handler')
      shared.writable = true
      press('发送回答'); press('发送回答')
      assert.equal(sent.length, 1, 'the busy ref blocks a second tap before React rerenders')
      assert.equal(sent[0][1], value)
      shared.pendingAnswers = [{ question_id: 'mobile', text: value }]
      assert.match(words(), /正在发送/)
      assert.doesNotMatch(words(), /没确认送达/)
      hold.reject(new Error('connection lost')); await settleComponent()
      assert.match(words(), /没确认送达/)
      assert.equal(render().some(({ type, props }) => type === 'TextInput' || props.accessibilityRole === 'radio'), false)
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
      const app = componentHarness(platform === 'web' ? './question-card.tsx' : '../../../../../../apps/mobile/src/features/chat/question-card.tsx', {
        '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
        '@/data/context': { useWorkspace: () => ({ members: [], bots: [] }) },
        '@/data/live': { useLive: () => shared },
        '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { principal: { device_id: 'self', source: 'membership', can_manage: false } } }) },
        '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备' },
        '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, questionMeta, questionParts, settledAnswer, sharedProject },
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
  const work = { resolved: { provider: 'codex', model: null }, trigger: 'message:one', target_node: 'offline', cwd: '/project', title: '检查项目', prompt: '', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null }
  const data = parseConversation({ ...context, works: [{ ...work, work_id: 'held', thread_id: 'session' }, { ...work, work_id: 'running', target_node: 'online' }], questions: [{ question_id: 'approval', conversation_id: 'chat', bot: null, severity: 'attention', text: '继续吗？', choices: [{ label: '继续' }], about: { node: 'offline', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' }, author: 'offline', created_at_ms: 1, answer: null, closed: null }] })
  const works = data.works.map((item) => ({ ...item, conversation_id: 'chat' }))
  const nodes = parseNodes({ nodes: [{ device_id: 'offline', name: '离线电脑', online: false, local: false, revoked: false }, { device_id: 'online', name: '在线电脑', online: true, local: false, revoked: false }] })
  const overview = workspaceModel.overviewFromWorkspace({ openWorks: works, bots: [], questions: [], nodes })
  const workspace = { nodes, openWorks: works, bots: [], questions: data.questions, overview, selectedBot: null, data, conversationId: 'chat', synced: true, connectionOnline: true, answered: {}, liveProgress: [] }
  const checks = workspaceModel.workChecks(works, nodes)
  const app = componentHarness('./tasks-overview.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/workspace/labels.ts': taskLabels,
    './inbox': { useInbox: () => [{ kind: 'question', done: false, question: data.questions[0] }, ...checks.map((check) => ({ kind: 'confirm', check }))] },
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
  const data = { local_node: 'worker', works: [] as unknown[], questions: [] }
  const workspace = { generation: 1, nodes: [], liveProgress: [], writable: true, data, interruptWork: async (work: string) => { stopped.push(work) } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { ...taskLabels, nodeName: () => '设备', promptSummary, taskActive: () => true, taskLabel: () => '', taskStale: () => false, staleTaskLabel: '', progressLabel: () => '', shownState: (work: { state: string }) => work.state, taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': workspaceQuestions }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
  const stop = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'destructive')
  const ask = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'outline' && [props.children].flat().some((child) => typeof child === 'string' && child.includes('停止')))
  try {
    const work = { work_id: 'work', target_node: 'worker', cwd: '/p', title: '工作', prompt: '工作', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null }
    data.works = [{ ...work, state: 'unverified' }]
    assert.equal(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' })), undefined)
    data.works = [{ ...work, target_node: 'other', stop: 'stop_waiting' }]
    assert.equal(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' })), undefined, "a stop already on its way is not sent twice")
    data.works = [{ ...work, target_node: 'other' }]
    const first = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.equal(stop(first), undefined, 'one tap never stops a task')
    ;(ask(first)?.props.onClick as () => void)()
    ;(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' }))?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work'])
    const words = (elements: { props: Record<string, unknown> }[]) => elements.flatMap(({ props }) => [props.children].flat()).filter((child) => typeof child === 'string' || typeof child === 'number').join('')
    const turn = { thread_id: 'th', turn_id: 'tu' }
    data.works = [{ ...work, ...turn }, { ...work, ...turn, work_id: 'steered' }]
    const elements = app.render('TaskDetail', { botId: 'bot', workId: 'work' })
    assert.match(words(elements), /会一起停止/, 'a shared turn lists what stops with it')
    assert.match(words(elements), /一起停止 2 个任务/)
    ;(ask(elements)?.props.onClick as () => void)()
    ;(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' }))?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work', 'work'], 'the count is named before the confirming tap stops them all')
  } finally { app.close() }
})

test("below lg the task panel is a modal dialog that makes the rest of the page inert, and beside the chat it is not", () => {
  const workspace = { generation: 1, nodes: [], liveProgress: [], writable: true, data: { local_node: 'worker', questions: [], works: [{ work_id: 'work', target_node: 'worker', cwd: '/p', title: '整理日志', prompt: '', state: 'completed', summary: '', outcome: 'completed', result_text: '好了', failure_reason: null }] } }
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
  const workspace = () => { const owner = generation; return { generation: owner, isCurrent: () => owner === generation, nodes: [], pendingBot: null, writable: true, createBot: () => work.promise } }
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
    assert.equal(render().some(({ props }) => props.role === 'alert'), false)
    assert.deepEqual(closed, [])
    work.resolve({}); await settleComponent()
    assert.deepEqual(closed, [false])
  } finally { app.close() }
})

test("conversation keeps unknown and newly edited drafts across connection changes", async () => {
  let generation = 1, work = pendingResult()
  const workspace = () => { const owner = generation; return {
    generation: owner, isCurrent: () => owner === generation, conversationId: 'chat', fresh: new Set(), data: context, messagePending: null, messageBusy: false,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: () => work.promise, run: async (action: () => Promise<void>) => { try { await action() } catch {} },
  } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { agentName, awaitsAnswer, foldAnswers, questionOpen, sharedProject }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
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
  const workspace = () => { const owner = generation; return { generation: owner, isCurrent: () => owner === generation, connectionOnline: true,
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

for (const mode of ['restore']) test(`recovery ${mode} clears submitted passwords before waiting and keeps the chosen file and name`, async () => {
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
  const render = () => app.render('RecoveryControls', { only: mode })
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

test("conversation blocks sending until an online node can run the manager Agent", () => {
  let sent = 0, refreshed = 0
  const workspace = (nodes: unknown[]) => ({
    generation: 1, isCurrent: () => true, refresh: async () => { refreshed++ }, conversationId: 'chat', fresh: new Set(), data: context, messagePending: null, messageBusy: false,
    chatSlot: { conversationId: 'chat', missing: false }, nodes, liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: async () => { sent++; return true }, run: async (action: () => Promise<void>) => { await action() },
  })
  let currentWorkspace = workspace([{ device_id: 'gateway', online: false, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }])
  const app = componentHarness('../chat/conversation-screen.tsx', { '@vibeface/client/workspace/labels.ts': taskLabels, '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { agentName, awaitsAnswer, foldAnswers, questionOpen, sharedProject }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const composer = () => app.render('ConversationScreen').find(({ type }) => type === 'PromptInput')?.props
  try {
    composer()?.onChange?.('hello'); composer()?.onSubmit?.()
    assert.equal(composer()?.disabled, true)
    assert.match(String(composer()?.reason), /运行 Bot/)
    assert.equal(sent, 0)
    assert.ok(refreshed > 0, "a bot no device can reply for reads readiness again")
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'needs_setup', roots: [] } }])
    assert.equal(composer()?.disabled, true, "a bot switch without a working provider cannot send")
    assert.equal(composer()?.hint, '', "an empty chat offers setup in its middle, not again under the box")
    assert.equal(composer()?.reason, '运行 Bot 的设备需设置', "the greyed box still says why")
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'ready', roots: [] } }])
    assert.equal(composer()?.disabled, false)
    composer()?.onSubmit?.()
    assert.equal(sent, 1)
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex', manager_state: 'unverified', roots: [] } }])
    assert.equal(composer()?.disabled, false, 'unknown authentication can be tried without claiming readiness')
    composer()?.onSubmit?.()
    assert.equal(sent, 2)
    currentWorkspace.data = { ...context, conversation: { ...context.conversation, owner: 'away' } }
    assert.equal(app.render('ConversationScreen').some(({ props }) => String(props.title).includes('离线，对话在等它')), true, 'an unverified local provider still offers to take over from an offline owner')
  } finally { app.close() }
})

test("an owner's reply on its way keeps its streamed text until the reply lands", () => {
  const job = { trigger: "message:m1", state: "publishing", message_id: "m1", reply_message_id: "r1", source_node: "owner" }
  const ask = { message_id: "m1", event_hash: "m1", role: "user", text: "hi", created_at_ms: 1 }
  const progress = { kind: "manager", key: "live", conversation_id: "chat", trigger: "message:m1", source_node: "owner", sequence: 1, text: "partial answer", items: [], needsRefresh: false }
  let messages: object[] = [ask]
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({
    generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), data: { ...parseConversation(context, "chat"), messages, scheduler: { jobs: [job], unanswered: [] } }, messagePending: null, messageBusy: false,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [progress], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    refresh: async () => {},
  }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { agentName, awaitsAnswer, foldAnswers, questionOpen, sharedProject }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const streamed = () => (app.render('ConversationScreen').find(({ type }) => type === 'Chat')?.props.entries as { message?: { state?: string; text: string } }[]).filter((entry) => entry.message?.state === 'streaming').map((entry) => entry.message?.text).join()
  try {
    assert.equal(streamed(), "partial answer")
    messages = [ask, { message_id: "r1", event_hash: "r1", role: "assistant", text: "full answer", created_at_ms: 2, in_reply_to: "m1" }]
    assert.equal(streamed(), "")
  } finally { app.close() }
})

test("a closed question neither waits on the person nor hides 思考中", () => {
  const job = { trigger: "message:m1", state: "running", message_id: "m1" }
  const ask = { message_id: "m1", event_hash: "m1", role: "user", text: "hi", created_at_ms: 1 }
  const question = (question_id: string, closed: string | null) => ({ question_id, conversation_id: "chat", bot: null, severity: "attention", text: "允许吗？", choices: [],
    about: { node: "gateway", agent: "codex", session_id: "s", cwd: "/w", summary: "", request_id: "r" }, author: "gateway", created_at_ms: 2, answer: null, closed })
  let questions = [question("lost", "request_lost")]
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({
    generation: 1, isCurrent: () => true, conversationId: 'chat', fresh: new Set(), data: { ...parseConversation(context, "chat"), messages: [ask], questions, scheduler: { jobs: [job], unanswered: [] } }, messagePending: null, messageBusy: false,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true, answered: {},
    refresh: async () => {},
  }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { agentName, awaitsAnswer, foldAnswers, questionOpen, sharedProject }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
  const thinking = () => (app.render('ConversationScreen').find(({ type }) => type === 'Chat')?.props.entries as { key: string }[]).some((entry) => entry.key === 'activity')
  try {
    assert.equal(thinking(), true, "a question its device closed is not the bot's status")
    questions = [question("open", null)]
    assert.equal(thinking(), false, "an open question is the status already")
  } finally { app.close() }
})

test("an outstanding clipboard permission cannot keep the add-device sheet busy or release another action", async () => {
  let generation = 1, creation = pendingResult()
  const invitation = { link: 'https://node.example/join#vibeface1test', qr: 'https://node.example/join#vibeface1test', publicEntry: true, expiresAt: Date.now() + 600_000 }
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
    assert.equal(render().some(({ props }) => props.children === '已复制'), false)
    creation.resolve(invitation); await settleComponent()
    assert.equal(labelled('复制链接')?.disabled, false)
  } finally { app.close() }
})

test("a link a phone's browser cannot open still gets a QR code, for the app", async () => {
  const shown = async (link: string, qr: string, publicEntry: boolean) => {
    const encoded: string[] = []
    const device = { connectionGeneration: () => 1, ready: { isCurrent: () => true, principal: { can_manage: true, network_id: 'network' }, entry: { kind: 'gateway', address: 'http://127.0.0.1:4790', device_id: 'target' } }, joins: [], createInvitation: async () => ({ link, qr, publicEntry, expiresAt: Date.now() + 600_000 }), refreshDirectory: async () => {} }
    const app = componentHarness('../devices/add-device.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device }, './join': { JoinRequests: () => null, useOpenJoins: () => [] }, '@/lib/glossary': { terms: { safetyCode: { label: '安全码', icon: 'ScanEye' } } },
      '@vibeface/client/lib/node.ts': { messageOf: (cause: Error) => cause.message }, 'qrcode-generator': () => ({ addData(value: string) { encoded.push(value) }, make() {}, createDataURL: () => 'data:qr' }),
    }, { setInterval: () => 0, clearInterval: () => {} })
    try {
      app.render('AddDevice'); app.render('AddDevice').find(({ props }) => [props.children].flat().includes('生成链接'))?.props.onClick?.(); await settleComponent()
      return { tree: app.render('AddDevice'), encoded }
    } finally { app.close() }
  }
  const local = await shown('http://127.0.0.1:4790/join#vibeface1test', 'vibeface://join?link=vibeface1test', false)
  assert.deepEqual(local.encoded, ['vibeface://join?link=vibeface1test'])
  assert.equal(local.tree.find(({ type }) => type === 'img')?.props.alt, '用手机 App 扫码')
  assert.ok(local.tree.some(({ type, props }) => type === 'p' && [props.children].flat().includes('用手机 App 扫码')))
  const open = await shown('https://node.example/join#vibeface1test', 'https://node.example/join#vibeface1test', true)
  assert.deepEqual(open.encoded, ['https://node.example/join#vibeface1test'])
  assert.equal(open.tree.find(({ type }) => type === 'img')?.props.alt, '用手机相机扫码打开')
  assert.equal(open.tree.some(({ type, props }) => type === 'p' && [props.children].flat().includes('用手机 App 扫码')), false)
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
