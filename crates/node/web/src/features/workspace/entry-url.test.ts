import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, extname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import type { ReadyConnection } from "../../lib/node.ts"
import type { WorkRecord } from "./types.ts"
import { approvalKind, approvalRisk, nodeName } from "./labels.ts"
import { inputText, parseArtifactPage, parseConversation, parseNodes, parseCreateBot, parseBot } from "./model.ts"
import { canSend, composerHint } from "../chat/protocol.ts"
import { primaryLane } from "../identity/connection-lane.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test", owner: "gateway" }, messages: [], autonomy: { mode: "report", version: 0 },
  works: [], pending_checks: [], local_node: "gateway",
}
test("conversation navigation stays in the trusted web and carries no credentials", () => {
})
test("malformed conversation snapshots fail closed instead of authorizing writes", () => {
  assert.equal(parseConversation(context, "chat").conversation?.owner, "gateway")
  assert.throws(() => parseConversation(context, "other"))
  assert.throws(() => parseConversation({ ...context, works: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...context, local_node: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...context, autonomy: undefined }, "chat"))
  assert.throws(() => parseConversation({ ...context, autonomy: { mode: "always", version: 1 } }, "chat"))
  assert.throws(() => parseConversation({ ...context, autonomy: { mode: "off", version: null } }, "chat"))
})
test("nodeName prefers rpc and directory names over truncated peer ids", () => {
  const id = "a".repeat(64)
  assert.equal(nodeName(id, { nodes: [{ device_id: id, name: "Mac Studio", local: false, online: true, revoked: false, roles: [] }] }), "Mac Studio")
  assert.equal(nodeName(id, { directory: { network_id: "n", epoch: 1, heads: [], members: [{ device_id: id, kind: "node", name: "工作机", manage: false }], revoked: [] } }), "工作机")
  assert.equal(nodeName(id), `${id.slice(0, 10)}…`)
})

test("capabilities parse the actual provider name and reject malformed node lists", () => {
  const nodes = parseNodes({ nodes: [{ device_id: "n", name: "本机", local: true, online: true, revoked: false, roles: ["scheduler"], capabilities: { manager: "cursor_agent", roots: ["/project"] } }] })
  assert.equal(nodes[0]?.name, "本机")
  assert.equal(nodes[0]?.capabilities?.manager, "cursor_agent")
  assert.throws(() => parseNodes({ nodes: [{ device_id: "n" }] }))
})
test("works, approvals with decline, reply status and pending checks keep their exact shape", () => {
  const work = { work_id: "work", trigger: "message:one", target_node: "worker", cwd: "/project", prompt: "检查项目", state: "waiting_approval", summary: "等待决定", outcome: null, result_text: "", failure_reason: null,
    open_approvals: [{ request_id: "req", kind: "commandExecution", summary: "运行命令", form: { command: "npm test", cwd: "/project", files: ["a.ts"], choices: [{ label: "拒绝", response: { decision: "decline" } }] } }],
    answered_approvals: [{ request_id: "old", kind: "commandExecution", summary: "运行命令", decision: "accept" }] }
  const scheduler = { jobs: [{ trigger: "message:one", state: "queued", kind: "human", sequence: 0, message_id: "one", read_only: true }], capabilities: { streaming: true, cancel: false, steer: false } }
  const message = { message_id: "reply", event_hash: "event", role: "assistant", text: "部分回复", created_at_ms: 1, in_reply_to: "one", status: "stopped", blobs: [{ hash: "result", size: 20 }] }
  const parsed = parseConversation({ ...context, works: [work], scheduler, messages: [message] })
  assert.deepEqual(parsed.works[0].open_approvals[0].form?.choices?.[0].response, { decision: "decline" })
  assert.equal(parsed.works[0].state, "waiting_approval")
  assert.deepEqual([parsed.works[0].open_approvals[0].form?.command, parsed.works[0].open_approvals[0].form?.files], ["npm test", ["a.ts"]])
  assert.equal(parsed.works[0].answered_approvals[0].decision, "accept")
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, answered_approvals: undefined }] }))
  assert.equal(parsed.scheduler?.jobs[0].read_only, true)
  assert.equal(parsed.messages[0].status, "stopped")
  assert.equal(parsed.messages[0].in_reply_to, "one")
  assert.deepEqual(parsed.messages[0].blobs, [{ hash: "result", size: 20 }])
  assert.equal(parseConversation(context).scheduler, null)
  for (const outcome of ["completed", "failed", "interrupted", "rejected", "unverified", "abandoned"]) {
    assert.equal(parseConversation({ ...context, works: [{ ...work, state: "finished", outcome, open_approvals: [] }] }).works[0].outcome, outcome)
  }
  assert.equal(parseConversation({ ...context, works: [{ ...work, state: undefined }] }).works[0].state, null)
  assert.equal(parseConversation({ ...context, works: [{ ...work, open_approvals: [{ request_id: "req", kind: "k", summary: "s" }] }] }).works[0].open_approvals[0].form, undefined)
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, state: "waiting" }] }))
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, open_approvals: undefined }] }))
  assert.throws(() => parseConversation({ ...context, messages: [{ ...message, status: "cancelled" }] }))
  const input = { request_id: "input", kind: "input", summary: "请补充需求", form: { questions: [{ id: "scope", header: "范围", question: "选择本次操作范围", options: [{ label: "当前目录", description: "仅处理当前目录" }] }] } }
  assert.equal(parseConversation({ ...context, works: [{ ...work, open_approvals: [input] }] }).works[0].open_approvals[0].form?.questions?.[0].options?.[0].label, "当前目录")
  assert.throws(() => parseConversation({ ...context, works: [{ ...work, open_approvals: [{ ...input, form: { questions: [...input.form.questions, ...input.form.questions] } }] }] }))
  const check = { check_id: "manager:attempt", kind: "manager_process", detail: "进程退出未确认", process_groups: [42] }
  assert.deepEqual(parseConversation({ ...context, pending_checks: [check] }).pending_checks, [check])
  assert.deepEqual(parseConversation({ ...context, pending_checks: [{ check_id: "work:w", kind: "work", detail: "无法证明结果", work_id: "w", target_node: "worker" }] }).pending_checks[0], { check_id: "work:w", kind: "work", detail: "无法证明结果", process_groups: [], work_id: "w", target_node: "worker" })
  assert.deepEqual(parseConversation({ ...context, works: [{ ...work, state: "waiting_for_device", stop: { waiting_for_device: true, timed_out: false } }] }).works[0].stop, { waiting_for_device: true, timed_out: false })
  assert.throws(() => parseConversation({ ...context, pending_checks: [{ ...check, kind: "native_turn" }] }))
  assert.throws(() => parseConversation({ ...context, scheduler: { ...scheduler, jobs: [{ ...scheduler.jobs[0], sequence: -1 }] } }))
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
  const work: WorkRecord = { work_id: "work", trigger: "t", target_node: "gateway", cwd: "/project", prompt: "工作", state: "waiting_approval", summary: "", outcome: null, result_text: "", failure_reason: null,
    open_approvals: [{ request_id: "req", kind: "fileChange", summary: "允许修改吗？", form: { choices: [{ label: "拒绝", response: { decision: "decline" } }] } }], answered_approvals: [] }
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
      if (["interrupt_work", "answer_approval", "resolve"].includes(method)) {
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
    await render().answerApproval("work", "req", { decision: "decline" })
    assert.deepEqual(controls, [
      { method: "interrupt_work", conversation_id: "chat", work_id: "work" },
      { method: "answer_approval", conversation_id: "chat", work_id: "work", request_id: "req", response: { decision: "decline" }, attestation },
    ])
    assert.equal(signed.at(-1), 'attestation|network|chat|{"type":"approval_answered","work_id":"work","request_id":"req","decision":"decline"}')
    await render().resolveCheck({ check_id: "work:work", kind: "work", detail: "", process_groups: [], work_id: "work" }, "completed", " 设备上已看到结果 ")
    assert.deepEqual(controls.pop(), { method: "resolve", conversation_id: "chat", check_id: "work:work", outcome: "completed", note: "设备上已看到结果", attestation })
    assert.equal(signed.at(-1), 'attestation|network|chat|{"type":"work_finished","work_id":"work","outcome":"completed","result_text":"设备上已看到结果"}', "the choice is signed as the settlement the Node writes")
    await assert.rejects(render().interruptWork("other-work"), /停不了/)
    await assert.rejects(render().answerApproval("work", "other", { decision: "accept" }), /找不到这个请求/)
    const beforeUnknown = controls.length
    failControl = true
    await assert.rejects(render().answerApproval("work", "req", { decision: "decline" }), /connection lost/)
    assert.equal(render().writable, false)
    await assert.rejects(render().answerApproval("work", "req", { decision: "decline" }), /补读/)
    failControl = false
    await render().refresh()
    assert.equal(controls.length, beforeUnknown + 1)
    assert.equal(render().writable, true)
    work.state = "finished"; work.outcome = "completed"
    await render().refresh()
    await assert.rejects(render().interruptWork("work"), /停不了/)
    await assert.rejects(render().answerApproval("work", "req", { decision: "decline" }), /找不到这个请求/)
    work.state = "waiting_approval"; work.outcome = null
    const choices = work.open_approvals[0].form
    work.open_approvals[0].form = { questions: [{ id: "secret", header: "密码", question: "请输入", is_secret: true }] }
    await render().refresh()
    await assert.rejects(render().answerApproval("work", "req", { decision: "accept" }), /执行设备上填写/)
    assert.equal(controls.length, beforeUnknown + 1)
    work.open_approvals[0].form = choices
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

test("approvals submit only the person's selected response and keep sensitive input on the device", async () => {
  const responses: unknown[][] = []
  const request = { request_id: "approval", kind: "item/commandExecution/requestApproval", summary: "运行命令", form: { command: "npm test", choices: [{ label: "批准一次", response: { decision: "accept" } }, { label: "拒绝", response: { decision: "decline" } }] } }
  const work = { work_id: "work", target_node: "worker", state: "waiting_approval" }
  const workspace = { generation: 1, writable: true, nodes: [{ device_id: 'worker', online: true, revoked: false }], run: (action: () => Promise<void>) => action(), answerApproval: async (...args: unknown[]) => { responses.push(args) } }
  class Answers { get(name: string) { return name === 'answer-0' ? '当前目录' : name === 'answer-1' ? '是' : null } }
  const app = componentHarness('./task-activity.tsx', { './context': { useWorkspace: () => workspace }, '../identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName: () => '设备', approvalRisk, approvalKind } }, { FormData: Answers })
  try {
    let elements = app.render('ApprovalCard', { work, request })
    assert.deepEqual(responses, [])
    await elements.find(({ props }) => props.children === '拒绝')?.props.action?.()
    assert.deepEqual(responses, [['work', 'approval', { decision: 'decline' }]])
    elements = app.render('ApprovalCard', { work: { ...work, state: 'finished' }, request })
    assert.equal(elements.find(({ props }) => props.children === '同意一次')?.props.disabled, true)
    elements = app.render('ApprovalCard', { work, request: { ...request, form: { choices: [{ label: "取消本轮", response: { decision: "cancel" } }] } } })
    assert.equal(elements.some(({ props }) => props.children === '拒绝并停止'), true)
    assert.equal(elements.some(({ props }) => props.children === '同意一次'), false)
    const scope = { id: 'scope', header: '范围', question: '选择范围', options: [{ label: '当前目录' }] }
    elements = app.render('ApprovalCard', { work, request: { ...request, form: { questions: [scope] } } })
    await elements.find(({ props }) => props.children === '当前目录')?.props.action?.()
    assert.deepEqual(JSON.parse(JSON.stringify(responses[1])), ['work', 'approval', { answers: { scope: { answers: ['当前目录'] } } }])
    elements = app.render('ApprovalCard', { work, request: { ...request, form: { questions: [scope, { id: 'go', header: '继续', question: '继续吗' }] } } })
    elements.find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: { reportValidity: () => true } })
    await settleComponent()
    assert.deepEqual(JSON.parse(JSON.stringify(responses[2])), ['work', 'approval', { answers: { scope: { answers: ['当前目录'] }, go: { answers: ['是'] } } }])
    elements = app.render('ApprovalCard', { work, request: { ...request, form: { questions: [{ id: 'secret', header: '密码', question: '请输入', is_secret: true }] } } })
    assert.equal(elements.some(({ props }) => props.action), false)
    assert.equal(elements.some(({ props }) => props.children === '到执行设备上填写'), true)
    // Other devices see only the summary; the answer form exists where the work runs.
    elements = app.render('ApprovalCard', { work, request: { ...request, form: undefined } })
    assert.equal(elements.some(({ props }) => props.action), false)
    assert.equal(elements.some(({ props }) => props.children === '到执行设备上处理'), true)
  } finally { app.close() }
})

test("approval risk names deletion, force push, permissions, downloads and folders outside the allowed roots", () => {
  const risk = (command: string, cwd = "/p") => approvalRisk({ command, cwd }, ["/p"])
  assert.equal(risk("npm test"), "")
  assert.equal(risk("rm -rf dist"), "会删除文件")
  assert.equal(risk("git push -f origin main"), "会覆盖远端的提交记录")
  assert.equal(risk("sudo make install"), "会改动权限")
  assert.equal(risk("pnpm add lodash"), "会从网络下载")
  assert.equal(risk("ls", "/etc"), "在可用文件夹之外")
  assert.equal(risk("ls", "/p/sub"), "")
})


test("any entrance stops an unfinished task once, even on another device", async () => {
  const stopped: string[] = []
  const data = { local_node: 'worker', works: [] as unknown[] }
  const workspace = { generation: 1, nodes: [], liveProgress: [], writable: true, data, interruptWork: async (work: string) => { stopped.push(work) } }
  const kit = new Proxy({ useAction: () => ({ busy: null, error: null, run: (_key: string, work: () => Promise<void>) => work() }) }, { get: (target, name) => (target as Record<string | symbol, unknown>)[name] ?? String(name) })
  const app = componentHarness('./task-detail.tsx', { '@tanstack/react-router': { useNavigate: () => () => {}, Link: 'Link' }, './context': { useWorkspace: () => workspace }, '@/components/app/kit': kit, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName: () => '设备', taskActive: () => true, taskLabel: () => '', progressLabel: () => '', decisionLabel: () => '', approvalKind: () => '', shownState: (work: { state: string }) => work.state } }, { window: { addEventListener() {}, removeEventListener() {} } })
  const stop = (elements: { props: Record<string, unknown> }[]) => elements.find(({ props }) => props.variant === 'destructive')
  try {
    const work = { work_id: 'work', target_node: 'worker', cwd: '/p', prompt: '工作', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null, open_approvals: [], answered_approvals: [] }
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
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './protocol': { canSend, composerHint } })
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
  const app = componentHarness('../chat/elements/artifact.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace } })
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
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './protocol': { canSend, composerHint } })
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
