import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import { entryAddress, entryJson, entryKey, parseEntry, parseInfo, providerIds, providerNames } from "@vibeface/client/lib/node.ts"
import type { ReadyConnection } from "@vibeface/client/lib/node.ts"
import type { WorkRecord } from "@vibeface/client/workspace/types.ts"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { botReadiness, inputText, parseArtifactPage, parseConversation, pendingChecks, parseNodes, parseCreateBot, parseBot, formatSize, attachmentName, signedOrder } from "@vibeface/client/workspace/model.ts"
import { folderLabels, folderSetup, placeChanges, projectChoices, projectKey, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { agentName, answerLabel, attentionLabel, approvalTask, awaitsAnswer, declined, foldAnswers, projectName, questionMeta, questionOpen, settledAnswer, sharedProject, questionParts } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { followUp, followUpCopy, promptSummary } from "@vibeface/client/workspace/labels.ts"
import { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { setUpOnManager } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread } from "@vibeface/client/chat/timeline.ts"
import { primaryLane } from "@vibeface/client/identity/connection-lane.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test", owner: "gateway", owner_epoch: 1 }, messages: [],
  works: [], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [] }, sent: [], questions: [], sessions: [],
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
      if (specifier.endsWith("/lib/node.ts")) return { messageOf: (cause: unknown) => cause instanceof Error ? cause.message : String(cause), providerIds, providerNames }
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
  const connection: ReadyConnection = {
    generation: 1, entry: { kind: "gateway", device_id: "gateway", address: "https://node.example" },
    principal: { network_id: "network", device_id: "client", epoch: 1, can_manage: true, source: "membership" },
    info: { device_id: "gateway", name: "gateway", version: "test", initialized: true, network_id: "network" },
    isCurrent: () => activeConnection === connection, subscribe: () => () => {}, request: async (method: string) => { if (method === "write_state") return { state: "writable", conflicts: [] }; throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: [] }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: unreadNotifications }
      if (method === "questions") return { questions: questionRows }
      if (method === "open_works") return { works: [] }
      if (method === "answer") {
        answers.push(JSON.parse(JSON.stringify(body)))
        if (answerOutcome === "lost") throw new Error("connection lost")
        questionRows = questionRows.filter((row) => row.question_id !== body.question_id)
        return { status: answerOutcome, message: { message_id: `a-${body.question_id}`, role: "user", text: answerOutcome === "answered" ? body.text : "停下", answers: body.question_id } }
      }
      if (method === "ack") { acknowledgements.push(String(body.notification_id)); unreadNotifications = unreadNotifications.filter(item => item.notification_id !== body.notification_id); return {} }
      if (method === "conversation") return conversationHold ? conversationHold.promise : { ...context, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, messages: transcript, works: [work] }
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
  } finally {
    for (const effect of effects.values()) effect.cleanup?.()
  }
})

function componentHarness(path: string, dependencies: Record<string, unknown>, globals: Record<string, unknown> = {}) {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  type Props = Record<string, unknown> & { name?: string; type?: string; role?: string; disabled?: boolean; value?: unknown; onChange?: (event: unknown) => void; onSubmit?: (event?: unknown) => void; onClick?: () => void; action?: () => Promise<unknown> }
  const elements: { type: unknown; props: Props }[] = [], afterRender: (() => void)[] = []
  let cursor = 0
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
    useEffect: effect, useLayoutEffect: effect,
  }
  const jsx = (type: unknown, props: Props) => { const element = { type, props }; elements.push(element); return element }
  const ui = new Proxy({}, { get: (_target, name) => String(name) })
  const module: { exports: Record<string, unknown> } = { exports: {} }
  const require = (specifier: string) => specifier === 'react' ? react : specifier === 'react/jsx-runtime' ? { jsx, jsxs: jsx } : dependencies[specifier] ?? ui
  const filename = fileURLToPath(new URL(path, import.meta.url))
  const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
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
    close() { for (const effect of effects.values()) effect.cleanup?.() },
    module: module.exports,
  }
}

const pendingResult = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settleComponent = () => new Promise(setImmediate)

test("task filters keep the counts, device confirmations and empty state within the selected Bot", () => {
  const bots = [{ bot_id: "a", conversation_id: "chat-a", name: "Bot A" }, { bot_id: "b", conversation_id: "chat-b", name: "Bot B" }]
  const work = (conversation_id: string) => ({ kind: "work", work_id: conversation_id, check_id: conversation_id, conversation_id, target_node: "offline", title: conversation_id, detail: "", reason: "offline" })
  let inbox = [
    { key: "device", kind: "confirm", check: { kind: "device", works: [work("chat-a"), work("chat-b")] } },
    { key: "question-b", kind: "question", done: false, question: { conversation_id: "chat-b", bot: bots[1], about: {} } },
    { key: "process", kind: "confirm", check: { kind: "process", check_id: "process", detail: "still running" } },
  ]
  const workspace = { bots, selectedBot: bots[1], openWorks: [], nodes: [], questions: [], liveProgress: [], connectionOnline: true }
  const app = componentHarness("./tasks-overview.tsx", {
    "@vibeface/client/workspace/context.tsx": { useWorkspace: () => workspace },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
    "@vibeface/client/workspace/model.ts": workspaceModel,
    "./inbox": { useInbox: () => inbox },
    "./workspace-shell": { useCreateBot: () => () => {} },
  })
  const named = (elements: ReturnType<typeof app.render>, name: string) => elements.find(({ type }) => typeof type === "function" && type.name === name)
  try {
    let elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceMasthead")?.props.attention, 3)
    const selectBot = named(elements, "TaskFilters")?.props.onBotFilter
    assert.ok(typeof selectBot === "function")
    selectBot("a")
    elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceMasthead")?.props.attention, 1)
    const checks = elements.filter(({ type }) => type === "ConfirmCard").map(({ props }) => props.check)
    assert.deepEqual(JSON.parse(JSON.stringify(checks)), [{ kind: "device", works: [work("chat-a")] }])
    assert.equal(elements.some(({ type }) => type === "QuestionCard"), false)
    assert.equal(elements.find(({ props }) => props.role === "status")?.props.children, "1 项任务")
    inbox = inbox.filter((item) => item.kind === "question")
    elements = app.render("TasksOverview")
    assert.equal(named(elements, "WorkspaceMasthead")?.props.attention, 0)
    assert.equal(named(elements, "WorkspaceEmpty")?.props.title, "没有符合筛选的任务")
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
    assert.equal(attention(), 2)
    assert.equal(elements.some(({ type, props }) => type === "Link" && props.href === "/inbox"), true)
    assert.equal(empty(), false)
    inbox = []
    elements = app.render("TasksScreen")
    assert.equal(attention(), 0)
    assert.equal(empty(), true)
  } finally { app.close() }
})

test("question cards answer with a choice or typed text, retry an unconfirmed answer as sent and fold once answered", async () => {
  const sent: unknown[][] = []
  const workspace = { generation: 1, writable: true, nodes: [], answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], announce() {},
    bots: [{ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot' }], openWorks: [], data: { works: [{ work_id: 'w', thread_id: 's', title: '升级依赖', outcome: 'completed' }] },
    answer: async (...args: unknown[]) => { sent.push(args); return { status: 'answered', text: args[1] } } }
  const toasts: { text: string; action?: { label: string; onClick(): void } }[] = [], visited: unknown[] = []
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }), toast: (text: string, options: { action?: { label: string; onClick(): void } } = {}) => { toasts.push({ text, action: options.action }) } }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const router = { Link: 'Link', useNavigate: () => (target: unknown) => { visited.push(target) } }
  const app = componentHarness('./question-card.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { nodeName: () => '工作电脑' }, '@vibeface/client/workspace/questions.ts': { agentName, answerLabel, attentionLabel, approvalTask, declined, questionMeta, questionParts, settledAnswer, sharedProject }, '@/components/app/kit': kit, '@tanstack/react-router': router })
  const question = { question_id: 'q1', conversation_id: 'chat', bot: { bot_id: 'bot', name: '项目 Bot' }, severity: 'attention', text: '要不要继续', choices: [{ label: '继续' }, { label: '停下' }], about: { node: 'worker', agent: 'codex', session_id: 's', cwd: '/path/to/web/', summary: '改登录' }, author: 'gateway', created_at_ms: 1, answer: null }
  type Element = { type: unknown; props: Record<string, unknown> & { onClick?: () => void; onChange?: (event: unknown) => void; onSubmit?: (event: unknown) => void } }
  const text = (node: unknown): string => typeof node === 'string' ? node : Array.isArray(node) ? node.map(text).join('') : node && typeof node === 'object' && 'props' in node ? text((node as Element).props.children) : ''
  const labelled = (elements: Element[], label: string) => elements.find(({ props }) => props.onClick && text(props.children).includes(label))
  const words = (elements: Element[]) => elements.flatMap(({ props }) => [props.children].flat()).filter((child) => typeof child === 'string').join('')
  const plain = (value: unknown) => JSON.parse(JSON.stringify(value))
  try {
    let elements: Element[] = app.render('QuestionCard', { question })
    assert.match(words(elements), /web/, 'the meta line names the project')
    assert.match(words(elements), /工作电脑/, 'and the device')
    assert.equal(elements.some(({ type }) => type === 'Link'), false, 'in its own chat the card does not link to it')
    labelled(elements, '停下')?.props.onClick?.(); await settleComponent()
    assert.deepEqual(plain(sent[0]), [question, '停下'], 'a choice answers with its label')
    app.render('QuestionCard', { question }).find(({ props }) => props['aria-label'] === '其他回答')?.props.onChange?.({ target: { value: '先等等' } })
    app.render('QuestionCard', { question }).find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {} }); await settleComponent()
    assert.deepEqual(plain(sent[1]), [question, '先等等'], 'typed text answers too')
    ;(app.render('QuestionCard', { question }).find(({ props }) => props.role === 'group')?.props.onKeyDown as (event: unknown) => void)({ key: '1', preventDefault() {} }); await settleComponent()
    assert.deepEqual(plain(sent[2]), [question, '继续'], 'a focused choice answers by its number')
    elements = app.render('QuestionCard', { question, inbox: true })
    assert.equal(elements.find(({ type }) => type === 'Link')?.props.params && plain(elements.find(({ type }) => type === 'Link')?.props.params).botId, 'bot', '待处理 links to the chat')
    workspace.pendingAnswers = [{ question_id: 'q1', text: '继续' }]
    elements = app.render('QuestionCard', { question })
    assert.equal(labelled(elements, '停下'), undefined, 'an unconfirmed answer is not replaced by another')
    assert.match(words(elements), /没确认送达/)
    labelled(elements, '再试')?.props.onClick?.(); await settleComponent()
    assert.deepEqual(plain(sent[3]), [question, '继续'], 'trying again sends the same answer')
    workspace.pendingAnswers = []
    elements = app.render('QuestionCard', { question, answer: '继续' })
    assert.equal(elements.some(({ type }) => type === 'Button' || type === 'form'), false, 'an answered question offers nothing to press')
    assert.match(words(elements), /已回答 · 继续/)
    workspace.answered = { q1: '停下' }
    elements = app.render('QuestionCard', { question })
    assert.match(words(elements), /已回答 · 停下/, 'an answer that just landed folds the card at once')
    assert.equal(elements.some(({ props }) => props.role === 'status'), true, 'and is announced')
    assert.equal(toasts.length, 0, 'answering in the chat itself needs no toast')
    assert.doesNotMatch(words(elements), /升级依赖/, 'a question that held no task names none')
    elements = app.render('QuestionCard', { question: { ...question, about: { ...question.about, request_id: 'r' } } })
    assert.match(words(elements), /升级依赖[^]*工作电脑/, 'an answered approval names the task it held and its device, even once that task finished')
    workspace.answered = {}
    labelled(app.render('QuestionCard', { question: { ...question, question_id: 'q2' }, inbox: true }), '继续')?.props.onClick?.()
    await settleComponent()
    assert.deepEqual(toasts.map(({ text }) => text), ['已回答'], '待处理 says it was answered')
    toasts[0].action?.onClick()
    assert.deepEqual(plain(visited), [{ to: '/b/$botId', params: { botId: 'bot' } }], 'and leads to the chat it went to')
  } finally { app.close() }
})

test("any entrance stops an unfinished task once, after a confirming second tap, even on another device", async () => {
  const stopped: string[] = []
  const data = { local_node: 'worker', works: [] as unknown[], questions: [] }
  const workspace = { generation: 1, nodes: [], liveProgress: [], writable: true, data, interruptWork: async (work: string) => { stopped.push(work) } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备', promptSummary, taskActive: () => true, taskLabel: () => '', taskStale: () => false, staleTaskLabel: '', progressLabel: () => '', shownState: (work: { state: string }) => work.state, taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': { projectName, questionOpen } }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } })
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
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/workspace/labels.ts': { nodeName: () => '设备', promptSummary, taskActive: () => false, taskLabel: () => '', taskStale: () => false, staleTaskLabel: '', progressLabel: () => '', shownState: (work: { state: string }) => work.state, taskAgent: () => null, followUp, followUpCopy }, '@vibeface/client/lib/errors.ts': errors, '@vibeface/client/workspace/questions.ts': { projectName, questionOpen } }, { window: { addEventListener() {}, removeEventListener() {}, matchMedia: (query: string) => ({ matches: query === '(min-width: 1024px)' && wide }) } })
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
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', useDark: () => false, botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) }, '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/chat/protocol.ts': { awaitsDevice, botReach, botCanSend, botProviderNotice, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread }, '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks }, '@vibeface/client/workspace/questions.ts': { agentName, awaitsAnswer, foldAnswers, questionOpen, sharedProject }, '@vibeface/client/lib/errors.ts': errors }, { setInterval: () => 1, clearInterval() {} })
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
