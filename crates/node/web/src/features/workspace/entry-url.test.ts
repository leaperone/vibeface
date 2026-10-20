import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, extname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import type { ReadyConnection } from "../../lib/node.ts"
import type { WorkRecord } from "./types.ts"
import { nodeName } from "./labels.ts"
import { inputText, parseAlerts, parseArtifactPage, parseConversation, parseDecision, parseNodes, parseCreateBot, parseBot, formatSize } from "./model.ts"
import { placeChanges, routineName, routineTrigger } from "./assistant.ts"
import { canSend, composerHint } from "../chat/protocol.ts"
import { primaryLane } from "../identity/connection-lane.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test", owner: "gateway", owner_epoch: 1 }, messages: [],
  works: [], pending_checks: [], local_node: "gateway", changes: [], assistant: { skills: [], routines: [] },
}
test("conversation navigation stays in the trusted web and carries no credentials", () => {
})
test("malformed conversation snapshots fail closed instead of authorizing writes", () => {
  assert.equal(parseConversation(context, "chat").conversation?.owner, "gateway")
  assert.throws(() => parseConversation(context, "other"))
  assert.throws(() => parseConversation({ ...context, works: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...context, local_node: undefined }, "chat"))
})
test("nodeName prefers rpc and directory names and never shows a peer id", () => {
  const id = "a".repeat(64)
  assert.equal(nodeName(id, { nodes: [{ device_id: id, name: "Mac Studio", local: false, online: true, revoked: false, roles: [] }] }), "Mac Studio")
  assert.equal(nodeName(id, { directory: { network_id: "n", epoch: 1, heads: [], members: [{ device_id: id, kind: "node", name: "工作机", manage: false }], revoked: [] } }), "工作机")
  assert.equal(nodeName(id), "另一台设备")
})

test("capabilities parse the actual provider name and reject malformed node lists", () => {
  const nodes = parseNodes({ nodes: [{ device_id: "n", name: "本机", local: true, online: true, revoked: false, roles: ["scheduler"], capabilities: { manager: "cursor_agent", roots: ["/project"] } }] })
  assert.equal(nodes[0]?.name, "本机")
  assert.equal(nodes[0]?.capabilities?.manager, "cursor_agent")
  assert.throws(() => parseNodes({ nodes: [{ device_id: "n" }] }))
})
test("works, reply status and pending checks keep their exact shape", () => {
  const work = { work_id: "work", trigger: "message:one", target_node: "worker", cwd: "/project", title: "检查项目", prompt: "检查项目", state: "running", summary: "进行中", outcome: null, result_text: "", failure_reason: null }
  const scheduler = { jobs: [{ trigger: "message:one", state: "queued", message_id: "one" }], capabilities: { streaming: true, cancel: false, steer: false } }
  const message = { message_id: "reply", event_hash: "event", role: "assistant", text: "部分回复", created_at_ms: 1, in_reply_to: "one", status: "stopped", blobs: [{ hash: "result", size: 20 }] }
  const parsed = parseConversation({ ...context, works: [work], scheduler, messages: [message] })
  assert.equal(parsed.works[0].state, "running")
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "waiting_approval" }] }), "approval states are gone")
  assert.equal(parseConversation({ ...context, works: [{ ...work, title: undefined }] }).works[0].title, "", "a dispatch row without a title still parses")
  assert.equal(parsed.messages[0].status, "stopped")
  assert.equal(parsed.messages[0].in_reply_to, "one")
  assert.deepEqual(parsed.messages[0].blobs, [{ hash: "result", size: 20 }])
  assert.equal(parseConversation(context).scheduler, null)
  for (const outcome of ["completed", "failed", "interrupted", "rejected", "unverified", "abandoned"]) {
    assert.equal(parseConversation({ ...context, works: [{ ...work, state: "finished", outcome }] }).works[0].outcome, outcome)
  }
  assert.equal(parseConversation({ ...context, works: [{ ...work, state: undefined }] }).works[0].state, null)
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "waiting" }] }))
  assert.throws(() => parseConversation({ ...context, messages: [{ ...message, status: "cancelled" }] }))
  const check = { check_id: "manager:attempt", kind: "manager_process", detail: "进程退出未确认", process_groups: [42] }
  assert.deepEqual(parseConversation({ ...context, pending_checks: [check] }).pending_checks, [check])
  assert.deepEqual(parseConversation({ ...context, pending_checks: [{ check_id: "work:w", kind: "work", detail: "无法证明结果", work_id: "w", target_node: "worker" }] }).pending_checks[0], { check_id: "work:w", kind: "work", detail: "无法证明结果", process_groups: [], work_id: "w", target_node: "worker" })
  assert.deepEqual(parseConversation({ ...context, works: [{ ...work, state: "waiting_for_device", stop: { waiting_for_device: true, timed_out: false } }] }).works[0].stop, { waiting_for_device: true, timed_out: false })
  assert.throws(() => parseConversation({ ...context, pending_checks: [{ ...check, kind: "native_turn" }] }))
  assert.throws(() => parseConversation({ ...context, scheduler: { ...scheduler, jobs: [{ ...scheduler.jobs[0], state: 1 }] } }))
  assert.equal(parseConversation({ ...context, scheduler: { jobs: [{ trigger: "message:two", owner: "node", conversation_id: "c", state: "pending_delivery", message_id: "two" }] } }).scheduler?.jobs[0].state, "pending_delivery")
})
test("alerts and decision results fail closed on shapes the web cannot show", () => {
  const alert = { alert_id: "a", matter: "m", agent: "codex", session_id: "s", cwd: "/p", severity: "urgent", summary: "失败了", count: 1, node: "n", created_at_ms: 1, resolution: null }
  assert.deepEqual(parseAlerts({ alerts: [alert] })[0].choices, [], "an alert without choices takes a reply")
  assert.equal(parseAlerts({ alerts: [{ ...alert, choices: [{ label: "批准" }] }] })[0].choices[0].label, "批准")
  assert.throws(() => parseAlerts({ alerts: [{ ...alert, severity: "loud" }] }))
  assert.throws(() => parseAlerts({ alerts: [{ ...alert, resolution: "maybe" }] }))
  assert.equal(parseDecision({ decision_id: "d", thread_id: "t", status: "unknown" }).status, "unknown")
  assert.throws(() => parseDecision({ decision_id: "d", status: "sent" }))
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
    window: browser, URL, TextEncoder, Error, crypto: globalThis.crypto,
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
  const modules = new Map<string, { exports: Record<string, unknown> }>(), sourceRoot = fileURLToPath(new URL("../../", import.meta.url))
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
      if (specifier === "@/lib/node") return { messageOf: (cause: unknown) => cause instanceof Error ? cause.message : String(cause) }
      if (specifier === "react/jsx-runtime") return { jsx: () => null }
      if (specifier.endsWith("/generated/vibeface_secure.js")) return { attestationPayload: (network: string, scope: string, body: string) => `${network}|${scope}|${body}` }
      if (specifier === "@/lib/outbox") return { ...load(resolve(sourceRoot, "lib/outbox.ts")), indexedOutbox: () => memoryOutbox }
      if (specifier.endsWith("/transport.js")) return { signClient: async (domain: string, payload: string) => { signed.push(`${domain}|${payload}`); return "signature" } }
      const path = specifier.startsWith("@/") ? resolve(sourceRoot, specifier.slice(2)) : resolve(dirname(filename), specifier)
      return load(extname(path) ? path : `${path}.ts`)
    }
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(module.exports, require, module)
    return module.exports
  }
  type Workspace = ReturnType<typeof import("./context.tsx").useWorkspace>
  const workspaceModule = load(fileURLToPath(new URL("./context.tsx", import.meta.url)))
  assert.equal(typeof workspaceModule.useWorkspaceState, "function")
  const { useWorkspaceState } = workspaceModule as { useWorkspaceState(connection: ReadyConnection): Workspace }
  const bot = { bot_id: "bot", conversation_id: "chat", name: "助手", instructions: "处理项目", event_hash: "bot-event" }
  const work: WorkRecord = { work_id: "work", trigger: "t", target_node: "gateway", cwd: "/project", title: "工作", prompt: "工作", state: "running", summary: "", outcome: null, result_text: "", failure_reason: null }
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
  const alertRow = { alert_id: "alert", matter: "m", agent: "codex", session_id: "thread", choices: [{ label: "批准" }, { label: "拒绝" }], cwd: "/project", severity: "attention", summary: "要不要继续", count: 1, node: "worker", created_at_ms: 1, resolution: null }
  let alertRows: Record<string, unknown>[] = [alertRow]
  const decisions: Record<string, unknown>[] = []
  let decisionOutcome: "delivered" | "unknown" | "lost" = "unknown"
  let createMode: "unknown" | "rejected" | "success" = "unknown"
  let knownBots: ReturnType<typeof parseBot>[] = [bot]
  const connection: ReadyConnection = {
    generation: 1, entry: { device_id: "gateway", address: "https://node.example" },
    principal: { network_id: "network", device_id: "client", epoch: 1, can_manage: true, source: "membership" },
    info: { device_id: "gateway", name: "gateway", version: "test", roles: ["gateway"], initialized: true, network_id: "network" },
    isCurrent: () => activeConnection === connection, subscribe: () => () => {}, request: async (method: string) => { if (method === "write_state") return { state: "writable", conflicts: [] }; throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: [] }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: unreadNotifications }
      if (method === "alerts") return { alerts: alertRows }
      if (method === "send_decision") { decisions.push(JSON.parse(JSON.stringify(body))); if (decisionOutcome === "lost") throw new Error("connection lost"); return { decision_id: body.decision_id, thread_id: body.thread_id, status: decisionOutcome } }
      if (method === "resolve_alert") { controls.push(JSON.parse(JSON.stringify({ method, ...body }))); alertRows = alertRows.map((row) => row.alert_id === body.alert_id ? { ...row, resolution: body.resolution } : row); return { event_hash: "resolved" } }
      if (method === "ack") { acknowledgements.push(String(body.notification_id)); unreadNotifications = unreadNotifications.filter(item => item.notification_id !== body.notification_id); return {} }
      if (method === "conversation") return conversationHold ? conversationHold.promise : { ...context, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, messages: transcript, works: [work] }
      if (method === "create_bot") {
        creations.push(JSON.parse(JSON.stringify(body)))
        assert.deepEqual(botBody(JSON.parse(stored.get("vibeface.pending-bot.client.network") ?? "null")), creations.at(-1))
        if (createMode === "unknown") throw new Error("connection lost")
        if (createMode === "rejected") throw Object.assign(new Error("project unavailable"), { responded: true })
        const created = parseBot({ ...body, event_hash: "new-bot-event" })
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
    const state = useWorkspaceState(activeConnection)
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
    const [alert] = render().alerts
    assert.equal(alert.summary, "要不要继续")
    decisionOutcome = "lost"
    await assert.rejects(render().sendDecision(alert, "d1", { choice: 0 }), /connection lost/)
    decisionOutcome = "unknown"
    assert.equal((await render().sendDecision(alert, "d1", { choice: 0 })).status, "unknown")
    decisionOutcome = "delivered"
    assert.equal((await render().sendDecision(alert, "d1", { choice: 0 })).status, "delivered")
    assert.deepEqual(decisions.map((item) => item.decision_id), ["d1", "d1", "d1"], "a retry sends the same decision_id")
    assert.deepEqual(decisions[0], { node: "worker", thread_id: "thread", decision_id: "d1", alert_id: "alert", choice: 0 })
    await assert.rejects(render().sendDecision(alert, "d2", { text: "  " }), /不能为空/)
    await render().resolveAlert(alert, "decided")
    assert.deepEqual(controls.pop(), { method: "resolve_alert", alert_id: "alert", resolution: "decided", attestation })
    assert.equal(signed.at(-1), 'attestation|network|attention|{"type":"attention_resolved","alert_id":"alert","resolution":"decided"}', "the dismissal is signed for the shared attention scope")
    assert.equal(render().alerts[0].resolution, "decided")
    const resolved = controls.length
    await render().resolveAlert(alert, "dismissed")
    assert.equal(controls.length, resolved, "a resolved alert is not resolved twice")
    await render().resolveCheck({ check_id: "work:work", kind: "work", detail: "", process_groups: [], work_id: "work" }, "completed", " 设备上已看到结果 ")
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
    await assert.rejects(render().createBot({ name: "项目助手", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /connection lost/)
    const originalCreation = creations[0]
    assert.equal(typeof originalCreation.bot_id, "string")
    assert.notEqual(originalCreation.bot_id, originalCreation.conversation_id)
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    await reload()
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    await assert.rejects(render().createBot({ name: "改名", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /内容已保留/)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "项目助手", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /project unavailable/)
    assert.deepEqual(botBody(render().pendingBot), originalCreation)
    assert.equal(render().pendingBot?.submission?.state, "unconfirmed")
    createMode = "success"
    await render().createBot({ name: "项目助手", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } })
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
        assert.equal(render().error, "")
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
  const react = {
    useId: () => 'component',
    useState(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = typeof initial === 'function' ? initial() : initial
      return [values[index], (next: unknown) => { values[index] = typeof next === 'function' ? next(values[index]) : next }]
    },
    useRef(initial: unknown) { const index = cursor++; if (!(index in values)) values[index] = { current: initial }; return values[index] },
    useEffect(run: () => (() => void) | void, dependencies: unknown[]) {
      const index = cursor++, previous = effects.get(index)
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) afterRender.push(() => {
        previous?.cleanup?.(); effects.set(index, { dependencies, cleanup: run() ?? undefined })
      })
    },
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
  }
}

const pendingResult = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settleComponent = () => new Promise(setImmediate)

test("alert cards answer with a fixed choice or a short reply and keep the decision id until the session confirms", async () => {
  const sent: unknown[][] = [], resolved: unknown[][] = []
  let status = "unknown"
  const workspace = { generation: 1, writable: true, nodes: [], run: (action: () => Promise<void>) => action(), announce() {},
    sendDecision: async (...args: unknown[]) => { sent.push(args); return { decision_id: args[1], status } }, resolveAlert: async (...args: unknown[]) => { resolved.push(args) } }
  const app = componentHarness('./alert-card.tsx', { './context': { useWorkspace: () => workspace }, '../identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName: () => '设备' }, './storage': { newId: () => `id-${sent.length}` } })
  const alert = { alert_id: 'alert', matter: 'm', agent: 'codex', session_id: 'thread', choices: [{ label: '批准' }, { label: '拒绝' }], cwd: '/p', severity: 'attention', summary: '要不要继续', count: 2, node: 'worker', created_at_ms: 1, resolution: null }
  const button = (elements: { props: Record<string, unknown> }[], label: string) => elements.find(({ props }) => props.children === label)
  try {
    let elements = app.render('AlertCard', { alert })
    assert.equal(elements.some(({ type }) => type === 'form'), false, 'a decision with choices has no free text')
    await (button(elements, '拒绝')?.props.action as () => Promise<void>)()
    const plain = (value: unknown) => JSON.parse(JSON.stringify(value))
    assert.deepEqual(plain(sent[0]), [alert, 'id-0', { choice: 1 }])
    elements = app.render('AlertCard', { alert })
    assert.equal(elements.some(({ props }) => props.role === 'status'), true, 'an unconfirmed send says so')
    status = 'delivered'
    await (button(app.render('AlertCard', { alert }), '拒绝')?.props.action as () => Promise<void>)()
    assert.deepEqual(plain(sent[1]), [alert, 'id-0', { choice: 1 }], 'the same answer reuses the decision id')
    assert.deepEqual(resolved, [], 'the device resolves a delivered decision itself')
    elements = app.render('AlertCard', { alert: { ...alert, choices: [] } })
    assert.equal(elements.some(({ type }) => type === 'form'), true, 'a Codex session without choices takes a reply')
    elements = app.render('AlertCard', { alert: { ...alert, choices: [], agent: 'claude' } })
    assert.equal(elements.some(({ type }) => type === 'form'), false, 'a Claude Code session has no channel back')
    elements = app.render('AlertCard', { alert: { ...alert, resolution: 'dismissed' } })
    assert.equal(button(elements, '批准'), undefined, 'a handled alert offers no answers')
  } finally { app.close() }
})

test("any entrance stops an unfinished task once, even on another device", async () => {
  const stopped: string[] = []
  const data = { local_node: 'worker', works: [] as unknown[] }
  const workspace = { generation: 1, nodes: [], liveProgress: [], writable: true, data, interruptWork: async (work: string) => { stopped.push(work) } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, './context': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName: () => '设备', taskActive: () => true, taskLabel: () => '', progressLabel: () => '', shownState: (work: { state: string }) => work.state } }, { window: { addEventListener() {}, removeEventListener() {} } })
  const stop = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'destructive')
  try {
    const work = { work_id: 'work', target_node: 'worker', cwd: '/p', title: '工作', prompt: '工作', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null }
    data.works = [{ ...work, state: 'unverified' }]
    assert.equal(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' })), undefined)
    data.works = [{ ...work, target_node: 'other', stop: { waiting_for_device: true, timed_out: false } }]
    assert.equal(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' })), undefined, "a stop already on its way is not sent twice")
    data.works = [{ ...work, target_node: 'other' }]
    ;(stop(app.render('TaskDetail', { botId: 'bot', workId: 'work' }))?.props.onClick as () => void)()
    await settleComponent()
    assert.deepEqual(stopped, ['work'])
  } finally { app.close() }
})

test("bot projects use online manager capabilities and preserve device names", () => {
  const node = { device_id: 'worker', name: '工作电脑', local: false, online: true, revoked: false, roles: ['scheduler'], capabilities: { manager: 'codex', roots: ['/project'] } }
  const app = componentHarness('./bot-form.tsx', { '@tanstack/react-router': { useNavigate: () => () => {} }, './context': { useWorkspace: () => ({ generation: 1, nodes: [node, { ...node, device_id: 'no-agent', name: '无 Agent', capabilities: { manager: null, roots: ['/unavailable'] } }], pendingBot: null, writable: true }) }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName } })
  try {
    const projects = app.render('BotForm', { open: true, onOpenChange() {}, restoreFocus() {} }).filter(({ type, props }) => type === 'NativeSelectOption' && props.value)
    assert.equal(projects.length, 1)
    assert.equal(projects[0].props.children, '工作电脑 · /project')
  } finally { app.close() }
})

test("bot drafts survive a new connection while old create completion cannot close or unlock its form", async () => {
  let generation = 1, work = pendingResult()
  const closed: boolean[] = []
  const workspace = () => { const owner = generation; return { generation: owner, isCurrent: () => owner === generation, nodes: [], pendingBot: null, writable: true, createBot: () => work.promise } }
  let currentWorkspace = workspace()
  const app = componentHarness('./bot-form.tsx', { '@tanstack/react-router': { useNavigate: () => () => {} }, './context': { useWorkspace: () => currentWorkspace }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, '@/lib/node': { messageOf: (cause: Error) => cause.message } })
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
    generation: owner, isCurrent: () => owner === generation, conversationId: 'chat', data: context, messagePending: null, messageBusy: false,
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [{ device_id: 'gateway', online: true, capabilities: { manager: 'codex_sdk', roots: [] } }], liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: () => work.promise, run: async (action: () => Promise<void>) => { try { await action() } catch {} },
  } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './protocol': { canSend, composerHint }, '@/features/workspace/assistant': { placeChanges, routineName, routineTrigger } })
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
  const app = componentHarness('./present.tsx', { './context': { useWorkspace: () => ({ generation }) } })
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
    connectionGeneration: () => generation, entries: [{ address: 'https://node.example', device_id: 'target' }], initialInvitation: '',
    identity: { deviceId: 'browser' }, phase: 'disconnected', error: '',
    connect() { if (synchronousFailure) throw new Error('current invalid input'); generation++; return work.promise },
  }
  const app = componentHarness('../identity/connection-screen.tsx', { './context': { useDevice: () => device }, './connection-lane': { primaryLane }, '@/lib/node': { messageOf: (cause: Error) => cause.message }, '@/features/workspace/labels': { addressName: (address: string) => address } })
  const render = () => app.render('ConnectionScreen')
  const connect = () => render().find(({ props }) => props.children === '连接')?.props
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
  const app = componentHarness('../chat/elements/artifact.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace }, '@/features/workspace/model': { formatSize } })
  const render = () => app.render('Artifact', { hash: 'result', size: 5 })
  const open = () => render().find(({ props }) => props.children === '打开结果')?.props
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
  } finally { app.close() }
})

for (const mode of ['restore', 'passphrase']) test(`recovery ${mode} clears submitted fields before waiting and preserves replacement fields`, async () => {
  let generation = 1, work = pendingResult(), resets = 0
  class Input { value = 'test-only-value'; files: object[] = [{}]; checked = false }
  const fields = new Map(['bundle', 'password', 'old_password', 'confirmation', 'name', 'bootstrap', 'address', 'device_id', 'relay'].map(name => [name, new Input()]))
  const form = { elements: { namedItem: (name: string) => fields.get(name) }, reset() { resets++; for (const field of fields.values()) field.value = '' } }
  const device = { connectionGeneration: () => generation, ready: null, review: null, entry: null,
    beginRecovery: () => { generation++; return work.promise }, reencrypt: () => work.promise }
  const app = componentHarness('../identity/recovery-controls.tsx', {
    './context': { useDevice: () => device }, '@/lib/node': { parseEntry: (value: unknown) => value, messageOf: (cause: Error) => cause.message },
    '@/lib/secure': { passphrase: (value: string) => value }, '@/lib/device-name': { defaultBrowserName: () => 'browser' },
  }, { HTMLInputElement: Input, HTMLTextAreaElement: Input })
  const render = () => app.render('RecoveryControls', { only: mode })
  const submit = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: form })
  try {
    render(); render(); submit()
    assert.equal(resets, 1)
    const old = work
    generation++; render()
    const password = fields.get('password'); assert.ok(password); password.value = 'replacement test-only-value'
    old.reject(new Error('stale package failure')); await settleComponent()
    assert.equal(password.value, 'replacement test-only-value')
    assert.equal(resets, 1)
    assert.equal(render().some(({ props }) => props.error), false)
    work = pendingResult(); submit(); assert.equal(resets, 2)
    work.resolve(undefined); await settleComponent()
    assert.equal(render().find(({ type }) => type === 'fieldset')?.props.disabled, false)
    const bundle = fields.get('bundle'); assert.ok(bundle); bundle.files = []
    submit(); await settleComponent()
    assert.equal(resets, 3)
    assert.equal(render().find(({ props }) => props.error)?.props.error, '请选择恢复文件。')
  } finally { app.close() }
})

test("conversation blocks sending until an online node can run the manager Agent", () => {
  let sent = 0
  const workspace = (nodes: unknown[]) => ({
    generation: 1, isCurrent: () => true, conversationId: 'chat', data: context, messagePending: null, messageBusy: false,
    chatSlot: { conversationId: 'chat', missing: false }, nodes, liveProgress: [], interrupted: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { device_id: 'gateway' }, sendMessage: async () => { sent++; return true }, run: async (action: () => Promise<void>) => { await action() },
  })
  let currentWorkspace = workspace([{ device_id: 'gateway', online: false, capabilities: { manager: 'codex_sdk', roots: [] } }])
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './protocol': { canSend, composerHint }, '@/features/workspace/assistant': { placeChanges, routineName, routineTrigger } })
  const composer = () => app.render('ConversationScreen').find(({ type }) => type === 'PromptInput')?.props
  try {
    composer()?.onChange?.('hello'); composer()?.onSubmit?.()
    assert.equal(composer()?.disabled, true)
    assert.match(String(composer()?.hint), /运行助手/)
    assert.equal(sent, 0)
    currentWorkspace = workspace([{ device_id: 'gateway', online: true, capabilities: { manager: 'codex_sdk', roots: [] } }])
    assert.equal(composer()?.disabled, false)
    composer()?.onSubmit?.()
    assert.equal(sent, 1)
  } finally { app.close() }
})

test("an outstanding clipboard permission cannot keep the add-device sheet busy or release another action", async () => {
  let generation = 1, creation = pendingResult()
  const invitation = { link: 'https://node.example/join#vibeface1test', publicEntry: true, expiresAt: Date.now() + 600_000 }
  const clipboard = pendingResult()
  const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, principal: { can_manage: true, network_id: 'network' }, entry: { address: 'https://node.example', device_id: 'target' } } }
  const device = { connectionGeneration: () => generation, ready: ready(), joins: [], createInvitation: () => creation.promise, refreshDirectory: async () => {} }
  const app = componentHarness('../devices/add-device.tsx', {
    '@/features/identity/context': { useDevice: () => device }, '@/features/workspace/labels': { isLoopback: () => false }, './join': { JoinRequests: () => null, useOpenJoins: () => [] },
    '@/lib/node': { messageOf: (cause: Error) => cause.message },
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

test("device id boundaries require the device_id field", () => {
  const id = "ab".repeat(32), other = "cd".repeat(32)
  const transportSource = readFileSync(new URL("../../../transport.js", import.meta.url), "utf8")
    .replace(/^import[^\n]+\n/, "").replace(/\bexport /g, "")
  const transport = new Script(`${transportSource}\n({validEndpoint, validTarget})`).runInNewContext({ TextEncoder, TextDecoder, URL, location: { protocol: "https:" } })
  const source = readFileSync(new URL("../../lib/node.ts", import.meta.url), "utf8")
  const compiled = transpileModule(source, { compilerOptions: { module: ModuleKind.CommonJS } }).outputText
  const parsers = new Script(`(function(require, exports) { ${compiled}\nreturn exports })`).runInNewContext()(() => transport, {})
  assert.equal(parsers.parseEntry({ address: "https://node.example", device_id: id }).device_id, id)
  for (const relay of [false, true]) {
    const entry = parsers.parseEntry({ address: "https://node.example", device_id: id, relay })
    assert.equal(entry.device_id, id)
    assert.equal(Object.hasOwn(entry, "relay"), false)
  }
  assert.equal(parsers.parseInfo({ name: "node", version: "1", roles: ["replica"], initialized: true, network_id: other, device_id: id }).device_id, id)
  assert.equal(parseNodes({ nodes: [{ local: true, online: true, revoked: false, roles: ["replica"], device_id: id }] })[0].device_id, id)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", node_id: id }), /设备编号无效/)
  assert.throws(() => parsers.parseInfo({ name: "node", version: "1", roles: ["replica"], initialized: true, network_id: other, node_id: id }), /设备编号无效/)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", device_id: "invalid" }), /设备编号无效/)
  assert.throws(() => parseNodes({ nodes: [{ local: true, online: true, revoked: false, roles: [], node_id: id }] }), /缺少文字/)
})
