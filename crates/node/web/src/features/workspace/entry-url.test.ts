import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, extname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import type { ReadyConnection } from "../../lib/node.ts"
import type { NativeSessionRecord } from "./types.ts"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis, nodeName } from "./labels.ts"
import { hasQueuedMessage, isSettledManagerState, inputText, parseArtifactPage, parseConversation, parseNodes, parseCreateBot, parseBot } from "./model.ts"
import { canSend, composerHint } from "../chat/protocol.ts"
import { primaryLane } from "../identity/connection-lane.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test" }, messages: [], heads: ["head"],
  missing: { events: [], blobs: [], conflicts: [] }, agent_tasks: [], native_sessions: [], observation_targets: [],
}
test("conversation navigation stays in the trusted web and carries no credentials", () => {
  assert.equal(conversationHref("https://node.example/app?capability=secret&basis=%5B%5D#token", "next"), "/app?conversation=next")
  assert.deepEqual(mergeBasis(["a"], JSON.stringify(["b", "a"])), ["a", "b"])
  assert.deepEqual(mergeBasis(["a"], JSON.stringify(["ok", 1])), ["a"])
})
test("missing integrity fields fail closed instead of authorizing writes", () => {
  assert.equal(isIncomplete(parseConversation(context, "chat").missing), false)
  assert.equal(isIncomplete(undefined), true)
  assert.equal(isIncomplete({ events: [], blobs: [] }), true)
  assert.throws(() => parseConversation({ ...context, missing: { events: [], blobs: [] } }, "chat"))
  assert.throws(() => parseConversation({ ...context, heads: ["head", 2] }, "chat"))
  assert.throws(() => parseConversation(context, "other"))
  assert.equal(isIncomplete(parseConversation({ ...context, missing: { events: [], blobs: [{ hash: "blob" }], conflicts: [] } }).missing), true)
})
test("nodeName prefers rpc and directory names over truncated peer ids", () => {
  const id = "a".repeat(64)
  assert.equal(nodeName(id, { nodes: [{ peer_id: id, name: "Mac Studio", local: false, online: true, revoked: false, roles: [] }] }), "Mac Studio")
  assert.equal(nodeName(id, { directory: { network_id: "n", epoch: 1, heads: [], members: [{ subject_id: id, public_key: id, kind: "node", name: "工作机", node_roles: [], manage: false }], revoked: [] } }), "工作机")
  assert.equal(nodeName(id), `${id.slice(0, 10)}…`)
})

test("capabilities parse the actual provider name and reject malformed node lists", () => {
  const nodes = parseNodes({ nodes: [{ peer_id: "n", name: "本机", local: true, online: true, revoked: false, roles: ["scheduler"], capabilities: { manager: "cursor_agent", roots: ["/project"] } }] })
  assert.equal(nodes[0]?.name, "本机")
  assert.equal(nodes[0]?.capabilities?.manager, "cursor_agent")
  assert.throws(() => parseNodes({ nodes: [{ peer_id: "n" }] }))
})
test("only the original scheduler durable job settles a pending human message", () => {
  const pending = { message_id: "human-message", conversation_id: "chat", text: "work", scheduler_node: "scheduler", entry_node: "gateway", basis_heads: [] }
  const job = { trigger: "message:event", kind: "human", state: "queued", sequence: 0, message_id: pending.message_id, reply_message_id: "assistant-message", target_node: pending.scheduler_node }
  for (const state of ["queued", "claimed", "running", "cancelling", "completed", "cancelled", "failed", "unknown"]) assert.equal(hasQueuedMessage({ jobs: [{ ...job, state }] }, pending), true)
  for (const state of ["pending_delivery", "delivered", "rejected", "accepted", "toString", "__proto__", ""]) assert.equal(hasQueuedMessage({ jobs: [{ ...job, state }] }, pending), false)
  assert.equal(hasQueuedMessage(null, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ ...job, target_node: "other-scheduler" }] }, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ ...job, target_node: undefined }] }, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ ...job, message_id: "assistant-message" }] }, pending), false)
})
test("manager progress settlement preserves unknown results and delivery distinctions", () => {
  for (const state of ["completed", "failed", "cancelled", "unknown", "rejected"]) assert.equal(isSettledManagerState(state), true)
  for (const state of ["queued", "claimed", "running", "cancelling", "pending_delivery", "delivered", "accepted", "toString", ""]) assert.equal(isSettledManagerState(state), false)
})
test("agent tasks and native sessions keep terminal states and explicit human requests", () => {
  const task = { task_ref: "task", source_node: "source", target_node: "worker", origin_trigger: "message:one", state: "running", text: "检查项目", detail: "已接收" }
  const session = { work_id: "work", task_ref: "task", target_node: "worker", thread_id: "thread", turn_id: "turn", state: "waiting", freshness: "observed", can_control: true, detail: "等待决定", pending_requests: [{ request_id: 1, kind: "approval", text: "write file", choices: [{ label: "拒绝", response: { decision: "decline" } }] }] }
  const scheduler = { jobs: [{ trigger: "message:one", state: "queued", kind: "message", sequence: 0, message_id: "one", read_only: true }], capabilities: { streaming: true, cancel: false, steer: false }, write_frozen: true }
  const message = { message_id: "one", event_hash: "event", role: "assistant", text: "summary", created_at_ms: 1, blobs: [{ hash: "result", size: 20 }] }
  const parsed = parseConversation({ ...context, agent_tasks: [task], native_sessions: [session], observation_targets: ["worker"], scheduler, messages: [message] })
  assert.deepEqual(parsed.native_sessions[0].pending_requests[0].choices?.[0].response, { decision: "decline" })
  assert.equal(parsed.agent_tasks[0].task_ref, "task")
  assert.equal(parsed.native_sessions[0].task_ref, "task")
  assert.equal(parsed.native_sessions[0].freshness, "observed")
  assert.equal(parsed.native_sessions[0].can_control, true)
  assert.deepEqual(parsed.observation_targets, ["worker"])
  assert.equal(parsed.scheduler?.jobs[0].read_only, true)
  assert.deepEqual(parsed.messages[0].blobs, [{ hash: "result", size: 20 }])
  assert.equal(parseConversation(context).scheduler, null)
  for (const state of ["completed", "failed", "interrupted", "unknown"]) {
    const result = parseConversation({ ...context, agent_tasks: [{ ...task, state }], native_sessions: [{ ...session, state }] })
    assert.equal(result.agent_tasks[0].state, state)
    assert.equal(result.native_sessions[0].state, state)
  }
  for (const state of ["pending_delivery", "queued", "claimed", "accepted", "running", "cancelling", "completed", "failed", "cancelled", "interrupted", "unknown"]) {
    assert.equal(parseConversation({ ...context, agent_tasks: [{ ...task, state }] }).agent_tasks[0].state, state)
  }
  for (const state of ["running", "waiting", "interrupting", "completed", "failed", "interrupted", "unknown"]) {
    assert.equal(parseConversation({ ...context, native_sessions: [{ ...session, state }] }).native_sessions[0].state, state)
  }
  for (const state of [null, 1, {}, "toString", "__proto__", ""]) {
    assert.throws(() => parseConversation({ ...context, agent_tasks: [{ ...task, state }] }))
    assert.throws(() => parseConversation({ ...context, native_sessions: [{ ...session, state }] }))
  }
  assert.throws(() => parseConversation({ ...context, scheduler: { ...scheduler, jobs: [{ ...scheduler.jobs[0], sequence: -1 }] } }))
  assert.throws(() => parseConversation({ ...context, agent_tasks: [{ ...task, state: "waiting" }] }))
  assert.throws(() => parseConversation({ ...context, native_sessions: [{ ...session, state: "accepted" }] }))
  assert.throws(() => parseConversation({ ...context, native_sessions: [{ ...session, pending_requests: undefined }] }))
  for (const invalid of [{ freshness: undefined }, { freshness: "live" }, { can_control: undefined }, { can_control: "true" }]) {
    assert.throws(() => parseConversation({ ...context, native_sessions: [{ ...session, ...invalid }] }))
  }
  const cached = parseConversation({ ...context, native_sessions: [{ ...session, state: "unknown", freshness: "cached", can_control: false }] }).native_sessions[0]
  assert.equal(cached.work_id, "work")
  assert.equal(cached.thread_id, "thread")
  assert.equal(cached.turn_id, "turn")
  assert.equal(cached.freshness, "cached")
  assert.equal(cached.can_control, false)
  assert.deepEqual(cached.pending_requests[0].choices?.[0].response, { decision: "decline" })
  const input = { request_id: "input", kind: "input", text: "请补充需求", questions: [{ id: "scope", header: "范围", question: "选择本次操作范围", options: [{ label: "当前目录", description: "仅处理当前目录" }] }] }
  assert.equal(parseConversation({ ...context, native_sessions: [{ ...session, pending_requests: [input] }] }).native_sessions[0].pending_requests[0].questions?.[0].options?.[0].label, "当前目录")
  assert.throws(() => parseConversation({ ...context, native_sessions: [{ ...session, pending_requests: [{ ...input, questions: [...input.questions, ...input.questions] }] }] }))
  const local = parseConversation({ ...context, native_sessions: [{ ...session, task_ref: null, turn_id: null }] }).native_sessions[0]
  assert.equal(local.task_ref, null)
  assert.equal(local.turn_id, null)
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
    navigator: { locks: { request: async (_key: string, _options: unknown, run: (lock: object) => Promise<void>) => run({}) } },
    document: { hidden: false, addEventListener() {}, removeEventListener() {} },
  })
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
  const agentTask = { task_ref: "task", source_node: "source", target_node: "worker", origin_trigger: "message:one", state: "running", text: "工作", detail: "" }
  const session: NativeSessionRecord = { work_id: "work", task_ref: "task", target_node: "worker", thread_id: "thread", turn_id: "turn", state: "waiting", freshness: "observed", can_control: true, detail: "", pending_requests: [{ request_id: 7, kind: "approval", text: "允许修改吗？", choices: [{ label: "拒绝", response: { decision: "decline" } }] }] }
  let scheduler: Record<string, unknown> | null = null
  const deferred = () => {
    let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
    const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
    return { promise, resolve, reject }
  }
  let conversationHold: ReturnType<typeof deferred> | null = null, messageHold: ReturnType<typeof deferred> | null = null
  const acknowledgements: string[] = []
  let unreadNotifications: { notification_id: string; kind: string; text: string; conversation_id: string }[] = []
  const messages: Record<string, unknown>[] = [], controls: Record<string, unknown>[] = [], creations: Record<string, unknown>[] = []
  let failControl = false, failBotCheck = false
  let createMode: "unknown" | "rejected" | "success" = "unknown"
  let knownBots: ReturnType<typeof parseBot>[] = [bot]
  const connection: ReadyConnection = {
    generation: 1, entry: { peer_id: "gateway", address: "https://node.example" },
    principal: { network_id: "network", subject_id: "client", epoch: 1, can_manage: true, source: "membership" },
    info: { peer_id: "gateway", public_key: "key", version: "test", roles: ["gateway"], initialized: true },
    isCurrent: () => activeConnection === connection, subscribe: () => () => {}, request: async () => { throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: [] }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: unreadNotifications }
      if (method === "ack") { acknowledgements.push(String(body.notification_id)); unreadNotifications = unreadNotifications.filter(item => item.notification_id !== body.notification_id); return {} }
      if (method === "conversation") return conversationHold ? conversationHold.promise : { ...context, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, agent_tasks: [agentTask], native_sessions: [session] }
      if (method === "create_bot") {
        creations.push(JSON.parse(JSON.stringify(body)))
        assert.deepEqual(JSON.parse(JSON.stringify(parseCreateBot(JSON.parse(stored.get("vibeface.pending-bot.client.network") ?? "null")))), creations.at(-1))
        if (createMode === "unknown") throw new Error("connection lost")
        if (createMode === "rejected") throw Object.assign(new Error("project unavailable"), { responded: true })
        const created = parseBot({ ...body, event_hash: "new-bot-event" })
        knownBots = [...knownBots.filter((item) => item.bot_id !== created.bot_id), created]
        return { bot: created }
      }
      if (["cancel_agent_task", "native_interrupt", "native_respond"].includes(method)) {
        controls.push(JSON.parse(JSON.stringify({ method, ...body })))
        if (failControl) throw new Error("connection lost")
        return { accepted: true }
      }
      assert.equal(method, "message")
      messages.push(JSON.parse(JSON.stringify(body)))
      if (messageHold) return messageHold.promise
      scheduler = { jobs: [{ trigger: `message:${body.message_id}`, kind: "human", state: "pending_delivery", sequence: 0, message_id: body.message_id, target_node: "chosen" }] }
      return { accepted: true, queued: false, message_id: body.message_id, scheduler_node: "chosen" }
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
    render(); render()
    await new Promise(setImmediate)
  }
  try {
    render(); render()
    await new Promise(setImmediate)
    let state = render()
    assert.equal(state.writable, true)
    await state.refresh()
    state = render()
    assert.equal(state.selectedBot?.bot_id, "bot")
    await state.sendMessage("work")
    state = render()
    assert.ok(state.messagePending)
    const pending = JSON.parse(JSON.stringify(state.messagePending))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, text: "work", basis_heads: ["head"], scheduler_node: null, entry_node: "gateway", report_results: true })
    assert.equal(pending.scheduler_node, "chosen")
    await state.sendMessage("work")
    assert.deepEqual(messages[1], { ...messages[0], scheduler_node: "chosen" })
    await assert.rejects(render().sendMessage("different work"), /原消息/)
    connection.entry.peer_id = "other-gateway"
    await assert.rejects(render().sendMessage("work"), /原入口/)
    connection.entry.peer_id = "gateway"
    await state.cancelAgentTask("task")
    await render().interruptNative("work", "worker")
    await render().respondNative("work", "worker", 7, { decision: "decline" })
    assert.deepEqual(controls, [
      { method: "cancel_agent_task", conversation_id: "chat", task_ref: "task" },
      { method: "native_interrupt", conversation_id: "chat", work_id: "work", target_node: "worker" },
      { method: "native_respond", conversation_id: "chat", work_id: "work", target_node: "worker", request_id: 7, response: { decision: "decline" } },
    ])
    await assert.rejects(render().cancelAgentTask("other-task"), /未找到当前任务/)
    await assert.rejects(render().interruptNative("work", "other-worker"), /可中断/)
    await assert.rejects(render().respondNative("work", "worker", "7", { decision: "accept" }), /未找到当前待回复/)
    const beforeUnknown = controls.length
    failControl = true
    await assert.rejects(render().respondNative("work", "worker", 7, { decision: "decline" }), /connection lost/)
    assert.equal(render().writable, false)
    await assert.rejects(render().respondNative("work", "worker", 7, { decision: "decline" }), /补读/)
    failControl = false
    await render().refresh()
    assert.equal(controls.length, beforeUnknown + 1)
    assert.equal(render().writable, true)
    assert.equal(stored.has("vibeface.pending.client.network"), false)
    session.turn_id = null
    await render().refresh()
    await assert.rejects(render().interruptNative("work", "worker"), /可中断/)
    session.turn_id = "turn"; session.state = "unknown"
    await render().refresh()
    await assert.rejects(render().respondNative("work", "worker", 7, { decision: "decline" }), /未找到当前待回复/)
    session.state = "waiting"
    await render().refresh()
    session.pending_requests[0].questions = [{ id: "secret", header: "密码", question: "请输入", is_secret: true }]
    await render().refresh()
    await assert.rejects(render().respondNative("work", "worker", 7, { decision: "accept" }), /敏感输入/)
    assert.equal(controls.length, beforeUnknown + 1)
    delete session.pending_requests[0].questions
    session.freshness = "cached"; session.can_control = false; session.state = "unknown"; session.detail = "上次进展"
    await render().refresh()
    const retained = render().data?.native_sessions[0]
    assert.equal(retained?.work_id, "work")
    assert.equal(retained?.thread_id, "thread")
    assert.equal(retained?.turn_id, "turn")
    assert.equal(retained?.state, "unknown")
    assert.equal(retained?.freshness, "cached")
    assert.equal(retained?.can_control, false)
    assert.equal(retained?.detail, "上次进展")
    assert.equal(retained?.pending_requests[0].request_id, 7)
    session.state = "completed"
    await render().refresh()
    assert.equal(render().data?.native_sessions[0].state, "completed")
    session.freshness = "observed"; session.can_control = true; session.state = "waiting"; session.detail = ""
    await render().refresh()
    await assert.rejects(render().createBot({ name: "large", instructions: "中".repeat(2731) }), /8192/)
    assert.equal(render().pendingBot, null)
    await assert.rejects(render().createBot({ name: "n".repeat(512), instructions: "a".repeat(8192), project: { target_node: "executor", cwd: "/" + "p".repeat(4095) } }), /12000/)
    assert.equal(render().pendingBot, null)
    await assert.rejects(render().createBot({ name: "项目助手", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /connection lost/)
    const originalCreation = creations[0]
    assert.equal(typeof originalCreation.bot_id, "string")
    assert.notEqual(originalCreation.bot_id, originalCreation.conversation_id)
    assert.deepEqual(JSON.parse(JSON.stringify(parseCreateBot(render().pendingBot))), originalCreation)
    await reload()
    assert.deepEqual(JSON.parse(JSON.stringify(parseCreateBot(render().pendingBot))), originalCreation)
    await assert.rejects(render().createBot({ name: "改名", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /原配置/)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "项目助手", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } }), /project unavailable/)
    assert.deepEqual(JSON.parse(JSON.stringify(parseCreateBot(render().pendingBot))), originalCreation)
    assert.equal(render().pendingBot?.submission?.state, "unconfirmed")
    createMode = "success"
    await render().createBot({ name: "项目助手", instructions: "只处理当前项目", project: { target_node: "executor", cwd: "/project" } })
    assert.deepEqual(creations.slice(1), [originalCreation, originalCreation])
    assert.equal(render().pendingBot, null)
    createMode = "rejected"
    await assert.rejects(render().createBot({ name: "invalid project", instructions: "", project: { target_node: "executor", cwd: "/unavailable" } }), /已确认没有创建/)
    assert.equal(render().pendingBot, null)
    failBotCheck = true
    await assert.rejects(render().createBot({ name: "rejected with pending lookup", instructions: "" }), /lookup unavailable/)
    assert.equal(render().pendingBot?.submission?.state, "rejected")
    const creationsBeforeCheck = creations.length
    failBotCheck = false
    await reload()
    await assert.rejects(render().createBot({ name: "rejected with pending lookup", instructions: "" }), /已确认机器人没有创建/)
    assert.equal(creations.length, creationsBeforeCheck)
    assert.equal(render().pendingBot, null)
    createMode = "success"
    await render().createBot({ name: "corrected project", instructions: "" })
    assert.equal(render().pendingBot, null)
    assert.equal(render().selectedBot?.name, "corrected project")
    assert.equal(messages.length, 2)

    await t.test("new connections wait for their own complete read and retain unknown writes without replay", async () => {
      knownBots = [bot]
      scheduler = { jobs: [{ trigger: `message:${messages[0].message_id}`, kind: "human", state: "queued", sequence: 0, message_id: messages[0].message_id, target_node: "chosen" }] }
      await render().refresh()
      assert.equal(render().messagePending, null)
      for (const outcome of ["success", "failure"]) {
        const write = deferred(); messageHold = write
        const before = messages.length
        const sending = render().sendMessage("reconnect payload")
        const rejected = assert.rejects(sending, /切换|old response lost/)
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
        if (outcome === "success") reading.resolve({ ...context, scheduler: null, agent_tasks: [agentTask], native_sessions: [session], missing: { events: ["parent"], blobs: [], conflicts: [] } })
        else reading.reject(new Error("new read unavailable"))
        await new Promise(setImmediate)
        assert.equal(render().writable, outcome === "success")
        assert.equal(render().incomplete, true)
        assert.equal(acknowledgements.length, ackBefore)
        await assert.rejects(render().sendMessage("reconnect payload"), /补读|上下文完整/)
        conversationHold = null; scheduler = null
        await render().run(() => render().refresh())
        assert.equal(render().writable, true)
        assert.equal(render().incomplete, false)
        assert.deepEqual(JSON.parse(JSON.stringify(render().messagePending)), original)
        if (outcome === "success") write.resolve({ accepted: true, queued: true, message_id: original.message_id, scheduler_node: "chosen" })
        else write.reject(new Error("old response lost"))
        await rejected
        assert.equal(render().messageBusy, false)
        assert.equal(render().error, "")
        assert.deepEqual(JSON.parse(JSON.stringify(render().messagePending)), original)
        assert.equal(messages.length, before + 1)
      }
      messageHold = null
      const original = render().messagePending
      assert.ok(original)
      scheduler = { jobs: [{ trigger: `message:${original.message_id}`, kind: "human", state: "queued", sequence: 0, message_id: original.message_id, target_node: "chosen" }] }
      await render().refresh()
      assert.equal(render().messagePending, null)
      conversationHold = deferred()
      const next: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === next }
      activeConnection = next; render(); await new Promise(setImmediate)
      conversationHold.resolve({ ...context, scheduler: null, agent_tasks: [agentTask], native_sessions: [session], missing: { events: ["unrelated-parent"], blobs: [], conflicts: [] } })
      await new Promise(setImmediate)
      assert.equal(render().writable, true)
      assert.equal(render().incomplete, true)
      conversationHold = null
      await render().createBot({ name: "unrelated new bot", instructions: "" })
      assert.equal(render().selectedBot?.name, "unrelated new bot")
      const currentId = render().conversationId, ackBefore = acknowledgements.length
      unreadNotifications = [{ notification_id: "after-reconnect", kind: "result", text: "unread", conversation_id: currentId }]
      conversationHold = deferred()
      const restored: ReadyConnection = { ...connection, generation: activeConnection.generation + 1, isCurrent: () => activeConnection === restored }
      activeConnection = restored; render(); await new Promise(setImmediate)
      conversationHold.resolve({ ...context, conversation: { ...context.conversation, conversation_id: currentId }, scheduler: null, agent_tasks: [], native_sessions: [] })
      await new Promise(setImmediate)
      assert.equal(render().writable, true)
      assert.equal(render().incomplete, false)
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

test("native approvals submit only the person's selected response and keep sensitive input on the device", async () => {
  const responses: unknown[][] = []
  const request = { request_id: "approval", kind: "approval", text: "允许写入？", choices: [{ label: "允许", response: { decision: "accept" } }, { label: "拒绝", response: { decision: "decline" } }] }
  const session = { work_id: "work", target_node: "worker", freshness: "observed", can_control: true }
  class Answers { get(name: string) { return name === 'answer-0' ? '当前目录' : null } }
  const app = componentHarness('./task-activity.tsx', { './context': { useWorkspace: () => ({ generation: 1, writable: true, run: (action: () => Promise<void>) => action(), respondNative: async (...args: unknown[]) => { responses.push(args) } }) } }, { FormData: Answers })
  try {
    let elements = app.render('NativeApproval', { session, request, disabled: false })
    assert.deepEqual(responses, [])
    await elements.find(({ props }) => props.children === '拒绝')?.props.action?.()
    assert.deepEqual(responses, [['work', 'worker', 'approval', { decision: 'decline' }]])
    elements = app.render('NativeApproval', { session, request, disabled: true })
    assert.equal(elements.find(({ props }) => props.children === '允许')?.props.disabled, true)
    elements = app.render('NativeApproval', { session, request: { ...request, choices: undefined, questions: [{ id: 'scope', header: '范围', question: '选择范围', options: [{ label: '当前目录' }] }] }, disabled: false })
    elements.find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: { reportValidity: () => true } })
    await settleComponent()
    assert.deepEqual(JSON.parse(JSON.stringify(responses[1])), ['work', 'worker', 'approval', { answers: { scope: { answers: ['当前目录'] } } }])
    elements = app.render('NativeApproval', { session, request: { ...request, questions: [{ id: 'secret', header: '密码', question: '请输入', is_secret: true }] }, disabled: false })
    assert.equal(elements.some(({ props }) => props.action), false)
    assert.equal(elements.some(({ props }) => props.children === '此请求包含敏感输入，请在工作设备填写。'), true)
  } finally { app.close() }
})

test("task cancellation stays on its source or target while native interruption requires a known turn", async () => {
  const cancelled: string[] = []
  let peerId = 'source'
  const workspace = { generation: 1, nodes: [], liveProgress: [], writable: true, selectedBot: {}, connectionOnline: true, run: (action: () => Promise<void>) => action(), cancelAgentTask: async (task: string) => { cancelled.push(task) } }
  const app = componentHarness('./task-activity.tsx', { './context': { useWorkspace: () => workspace }, '../identity/context': { useDevice: () => ({ directory: null, ready: { entry: { peer_id: peerId } } }) }, './labels': { nodeName: () => '设备' } })
  try {
    const task = { task_ref: 'task', source_node: 'source', target_node: 'offline', state: 'pending_delivery', text: '工作', detail: '' }
    for (const entry of ['source', 'offline', 'observer']) {
      peerId = entry
      const elements = app.render('AgentTaskActivity', { task, sessions: [] })
      const cancel = elements.find(({ props }) => props.children === '请求取消任务')
      assert.equal(cancel?.props.disabled, entry === 'observer')
      assert.equal(elements.some(({ props }) => props.children === '请连接任务来源或目标设备后取消任务。'), entry === 'observer')
      await cancel?.props.action?.()
    }
    assert.deepEqual(cancelled, ['task', 'task'])
    const session = { work_id: 'work', target_node: 'worker', thread_id: 'thread', turn_id: null, state: 'running', freshness: 'observed', can_control: true, detail: '', pending_requests: [] }
    assert.equal(app.render('NativeSessionActivity', { session }).some(({ props }) => props.children === '停止原生工作'), false)
    assert.equal(app.render('NativeSessionActivity', { session: { ...session, turn_id: 'turn' } }).find(({ props }) => props.children === '停止原生工作')?.props.disabled, true)
  } finally { app.close() }
})

test("cached native progress preserves requests and terminal state while controls resume only after observation", async () => {
  const controls: string[] = []
  const request = { request_id: "approval", kind: "approval", text: "允许写入？", choices: [{ label: "允许", response: { decision: "accept" } }] }
  const session = { work_id: "work", target_node: "worker", thread_id: "thread", turn_id: "turn", state: "unknown", freshness: "cached", can_control: false, detail: "上次进展", pending_requests: [request] }
  const workspace = { generation: 1, nodes: [{ peer_id: 'worker', online: true, revoked: false }], liveProgress: [], writable: true, selectedBot: {}, connectionOnline: true,
    run: (action: () => Promise<void>) => action(), interruptNative: async () => { controls.push('interrupt') }, respondNative: async () => { controls.push('respond') } }
  const app = componentHarness('./task-activity.tsx', { './context': { useWorkspace: () => workspace }, '../identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName: () => '设备' } })
  try {
    let elements = app.render('NativeSessionActivity', { session })
    assert.equal(elements.some(({ props }) => props.children === '上次记录的状态与进展，当前待核对。确认最新进展后才能操作。'), true)
    assert.equal(elements.some(({ props }) => props.children === '上次进展'), true)
    assert.equal(elements.find(({ props }) => props.request === request)?.props.disabled, true)
    assert.equal(elements.some(({ props }) => props.children === '停止原生工作'), false)
    elements = app.render('NativeApproval', { session, request, disabled: false })
    assert.equal(elements.find(({ props }) => props.children === '允许')?.props.disabled, true)
    await elements.find(({ props }) => props.children === '允许')?.props.action?.()
    assert.deepEqual(controls, [])
    elements = app.render('NativeSessionActivity', { session: { ...session, state: 'completed' } })
    assert.equal(elements.find(({ type }) => type === 'StatusBadge')?.props.value, 'completed')
    assert.equal(elements.some(({ props }) => props.children === '上次进展'), true)
    const observed = { ...session, state: 'waiting', freshness: 'observed' }
    elements = app.render('NativeSessionActivity', { session: observed })
    assert.equal(elements.find(({ props }) => props.children === '停止原生工作')?.props.disabled, true)
    await elements.find(({ props }) => props.children === '停止原生工作')?.props.action?.()
    assert.deepEqual(controls, [])
    elements = app.render('NativeSessionActivity', { session: { ...observed, can_control: true } })
    assert.equal(elements.find(({ props }) => props.children === '停止原生工作')?.props.disabled, false)
    await elements.find(({ props }) => props.children === '停止原生工作')?.props.action?.()
    elements = app.render('NativeApproval', { session: { ...observed, can_control: true }, request, disabled: false })
    assert.equal(elements.find(({ props }) => props.children === '允许')?.props.disabled, false)
    await elements.find(({ props }) => props.children === '允许')?.props.action?.()
    assert.deepEqual(controls, ['interrupt', 'respond'])
  } finally { app.close() }
})

test("bot projects use online manager capabilities and preserve device names", () => {
  const node = { peer_id: 'worker', name: '工作电脑', local: false, online: true, revoked: false, roles: ['scheduler'], capabilities: { manager: 'codex', roots: ['/project'] } }
  const app = componentHarness('./bot-form.tsx', { './context': { useWorkspace: () => ({ generation: 1, nodes: [node, { ...node, peer_id: 'no-agent', name: '无 Agent', capabilities: { manager: null, roots: ['/unavailable'] } }], pendingBot: null, writable: true }) }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, './labels': { nodeName } })
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
  const app = componentHarness('./bot-form.tsx', { './context': { useWorkspace: () => currentWorkspace }, '@/features/identity/context': { useDevice: () => ({ directory: null }) }, '@/lib/node': { messageOf: (cause: Error) => cause.message } })
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
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { peer_id: 'gateway' }, sendMessage: () => work.promise, run: async (action: () => Promise<void>) => { try { await action() } catch {} },
  } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/conversation-screen.tsx', { '@/features/workspace/context': { useWorkspace: () => currentWorkspace }, './protocol': { canSend, composerHint } })
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
    connectionGeneration: () => generation, entries: [{ address: 'https://node.example', peer_id: 'target' }], initialInvitation: '',
    identity: { peerId: 'browser' }, phase: 'disconnected', error: '',
    connect() { if (synchronousFailure) throw new Error('current invalid input'); generation++; return work.promise },
  }
  const app = componentHarness('../identity/connection-screen.tsx', { './context': { useDevice: () => device }, './connection-lane': { primaryLane }, '@/lib/node': { messageOf: (cause: Error) => cause.message } })
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
    assert.equal(render().some(({ props }) => props.role === 'alert'), false)
    work.resolve(undefined); await settleComponent()
    assert.equal(connect()?.disabled, false)
    synchronousFailure = true; connect()?.onClick?.(); await settleComponent()
    assert.equal(render().find(({ props }) => props.role === 'alert')?.props.children, 'current invalid input')
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
  const fields = new Map(['bundle', 'password', 'old_password', 'confirmation', 'name', 'bootstrap', 'address', 'peer_id', 'relay'].map(name => [name, new Input()]))
  const form = { elements: { namedItem: (name: string) => fields.get(name) }, reset() { resets++; for (const field of fields.values()) field.value = '' } }
  const device = { connectionGeneration: () => generation, ready: null, review: null, entry: null,
    beginRecovery: () => { generation++; return work.promise }, reencrypt: () => work.promise }
  const app = componentHarness('../identity/recovery-controls.tsx', {
    './context': { useDevice: () => device }, '@/lib/node': { parseEntry: (value: unknown) => value, messageOf: (cause: Error) => cause.message },
    '@/lib/secure': { passphrase: (value: string) => value },
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
    assert.equal(render().some(({ props }) => props.role === 'alert'), false)
    work = pendingResult(); submit(); assert.equal(resets, 2)
    work.resolve(undefined); await settleComponent()
    assert.equal(render().find(({ type }) => type === 'fieldset')?.props.disabled, false)
    const bundle = fields.get('bundle'); assert.ok(bundle); bundle.files = []
    submit(); await settleComponent()
    assert.equal(resets, 3)
    assert.equal(render().find(({ props }) => props.role === 'alert')?.props.children, '请选择加密恢复包。')
  } finally { app.close() }
})

test("an outstanding clipboard permission cannot keep new device controls busy or release another action", async () => {
  let generation = 1, creation = pendingResult()
  const clipboard = pendingResult()
  const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, principal: { can_manage: true, network_id: 'network' }, entry: { address: 'https://node.example', peer_id: 'target' } } }
  const device = { connectionGeneration: () => generation, ready: ready(), identity: null, entries: [], directory: null, pairs: [], refreshDirectory: async () => {}, createInvitation: () => creation.promise }
  const app = componentHarness('../identity/device-controls.tsx', { './context': { useDevice: () => device } }, { navigator: { clipboard: { writeText: () => clipboard.promise } } })
  const render = () => app.render('DeviceControls')
  const generate = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {} })
  const button = () => render().find(({ props }) => props.children === '生成邀请')?.props
  try {
    render(); render().find(({ props }) => props.name === 'invite-kind' && props.value === 'node')?.props.onChange?.({})
    generate(); creation.resolve('test-only-invitation'); await settleComponent()
    const copy = render().find(({ props }) => props.children === '复制连接信息')
    assert.ok(copy?.props.onClick); copy.props.onClick()
    assert.equal(button()?.disabled, true)
    generation++; device.ready = ready(); render(); render()
    assert.equal(button()?.disabled, false)
    creation = pendingResult(); generate()
    clipboard.resolve(undefined); await settleComponent()
    assert.equal(button()?.disabled, true)
    assert.equal(render().some(({ props }) => props.children === '连接信息已复制。'), false)
    creation.resolve('new-test-only-invitation'); await settleComponent()
    assert.equal(button()?.disabled, false)
    assert.equal(render().some(({ props }) => props.children === '邀请已生成，请通过可信渠道发送。'), true)
  } finally { app.close() }
})

test("Peer ID boundaries require the peer_id field", () => {
  const id = "ab".repeat(32), other = "cd".repeat(32)
  const transportSource = readFileSync(new URL("../../../transport.js", import.meta.url), "utf8")
    .replace(/^import[^\n]+\n/, "").replace(/\bexport /g, "")
  const transport = new Script(`${transportSource}\n({validEndpoint, validTarget})`).runInNewContext({ TextEncoder, TextDecoder, URL, location: { protocol: "https:" } })
  const source = readFileSync(new URL("../../lib/node.ts", import.meta.url), "utf8")
  const compiled = transpileModule(source, { compilerOptions: { module: ModuleKind.CommonJS } }).outputText
  const parsers = new Script(`(function(require, exports) { ${compiled}\nreturn exports })`).runInNewContext()(() => transport, {})
  assert.equal(parsers.parseEntry({ address: "https://node.example", peer_id: id }).peer_id, id)
  for (const relay of [false, true]) {
    const entry = parsers.parseEntry({ address: "https://node.example", peer_id: id, relay })
    assert.equal(entry.peer_id, id)
    assert.equal(Object.hasOwn(entry, "relay"), false)
  }
  assert.equal(parsers.parseInfo({ public_key: other, version: "1", roles: ["replica"], initialized: true, peer_id: id }).peer_id, id)
  assert.equal(parsers.parseInvitation({ url: "https://node.example", capability: "bootstrap", peer_id: id }).entry.peer_id, id)
  assert.equal(parseNodes({ nodes: [{ local: true, online: true, revoked: false, roles: ["replica"], peer_id: id }] })[0].peer_id, id)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", node_id: id }), /64 位/)
  assert.throws(() => parsers.parseInfo({ public_key: other, version: "1", roles: ["replica"], initialized: true, node_id: id }), /64 位/)
  assert.throws(() => parsers.parseInvitation({ url: "https://node.example", capability: "bootstrap", node_id: id }), /64 位/)
  assert.throws(() => parsers.parseEntry({ address: "https://node.example", peer_id: "invalid" }), /64 位/)
  assert.throws(() => parseNodes({ nodes: [{ local: true, online: true, revoked: false, roles: [], node_id: id }] }), /文本字段/)
})
