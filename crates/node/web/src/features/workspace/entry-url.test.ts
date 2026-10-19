import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, extname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import type { ReadyConnection } from "../../lib/node.ts"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis } from "./labels.ts"
import { hasQueuedMessage, inputText, parseArtifactPage, parseConversation, parseNodes, parsePending, parseCreateBot, parseBot } from "./model.ts"
import { parseReconcile, reconcileDecision } from "./reconcile.ts"
import { canSend, composerHint } from "../chat/protocol.ts"
import { primaryLane } from "../identity/connection-lane.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test" }, messages: [], heads: ["head"],
  missing: { events: [], blobs: [], conflicts: [] }, executions: [], operations: [],
}
const request = { operation_id: "op", target_node: "executor", conversation_id: "chat", session_id: "session" }
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
test("capabilities parse the actual provider name and reject malformed node lists", () => {
  const nodes = parseNodes({ nodes: [{ node_id: "n", local: true, online: true, revoked: false, roles: ["scheduler"], capabilities: { manager: "cursor_agent", executors: ["codex"], roots: ["/project"] } }] })
  assert.equal(nodes[0]?.capabilities?.manager, "cursor_agent")
  assert.throws(() => parseNodes({ nodes: [{ node_id: "n" }] }))
})
test("only the exact executor operation can settle a pending write", () => {
  const operation = { request, state: "succeeded", result: {} }
  assert.equal(reconcileDecision(parseReconcile({ operation }), request), "settled")
  assert.equal(reconcileDecision(parseReconcile({ operation: { ...operation, state: "unknown" } }), request), "pending")
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", target_node: "executor", checked_at_ms: 100 }), request), "missing")
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", target_node: "other", checked_at_ms: 100 }), request), "pending")
  assert.throws(() => parseReconcile({ operation: null, state: "not_found", target_node: "executor" }))
  assert.throws(() => reconcileDecision(parseReconcile({ operation: { ...operation, request: { ...request, operation_id: "other" } } }), request))
})
test("only the original scheduler durable job settles a pending human message", () => {
  const pending = { message_id: "human-message", conversation_id: "chat", text: "work", scheduler_node: "scheduler", entry_node: "gateway", basis_heads: [] }
  const job = { trigger: "message:event", kind: "human", state: "queued", sequence: 0, message_id: pending.message_id, reply_message_id: "assistant-message", target_node: pending.scheduler_node }
  for (const state of ["queued", "claimed", "running", "cancelling", "completed", "cancelled", "failed", "unknown"]) assert.equal(hasQueuedMessage({ jobs: [{ ...job, state }] }, pending), true)
  for (const state of ["pending_delivery", "delivered", "rejected"]) assert.equal(hasQueuedMessage({ jobs: [{ ...job, state }] }, pending), false)
  assert.equal(hasQueuedMessage(null, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ ...job, target_node: "other-scheduler" }] }, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ ...job, target_node: undefined }] }, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ ...job, message_id: "assistant-message" }] }, pending), false)
})
test("execution requests, followup authorization and result references parse as explicit facts", () => {
  const execution = { operation_id: "execution", target_node: "executor", session_id: "session", revision: 4, title: "work", state: "running", session_state: "running", capabilities: ["respond_request"], pending_requests: [{ request_id: 1, kind: "approval", text: "write file", choices: [{ label: "拒绝", response: { decision: "decline" } }] }], freshness: "cached", checked_at_ms: 100, detail: "pending", blobs: [{ hash: "execution-result", size: 20 }], goal: { task_id: "work-goal", title: "goal", acceptance: "done", status: "working", followup_mode: "continue", followup_version: 3, followup_scheduler_node: "scheduler" } }
  const scheduler = { jobs: [{ trigger: "message:one", state: "queued", kind: "message", sequence: 0, message_id: "one", read_only: true }], capabilities: { streaming: true, cancel: false, steer: false }, write_frozen: true }
  const message = { message_id: "one", event_hash: "event", role: "assistant", text: "summary", created_at_ms: 1, blobs: [{ hash: "result", size: 20 }] }
  const parsed = parseConversation({ ...context, executions: [execution], scheduler, messages: [message] })
  assert.deepEqual(parsed.executions[0].pending_requests[0].choices?.[0].response, { decision: "decline" })
  assert.equal(parsed.executions[0].goal?.followup_version, 3)
  assert.deepEqual(parsed.executions[0].blobs, [{ hash: "execution-result", size: 20 }])
  assert.equal(parsed.scheduler?.jobs[0].read_only, true)
  assert.deepEqual(parsed.messages[0].blobs, [{ hash: "result", size: 20 }])
  assert.equal(parseConversation(context).scheduler, null)
  assert.throws(() => parseConversation({ ...context, scheduler: { ...scheduler, jobs: [{ ...scheduler.jobs[0], sequence: -1 }] } }))
  assert.throws(() => parseConversation({ ...context, executions: [{ ...execution, goal: { ...execution.goal, followup_mode: "approve_everything" } }] }))
  assert.throws(() => parseConversation({ ...context, executions: [{ ...execution, capabilities: undefined }] }))
  assert.throws(() => parseConversation({ ...context, executions: [{ ...execution, freshness: "live_because_online" }] }))
  const input = { request_id: "input", kind: "input", text: "请补充需求", questions: [{ id: "scope", header: "范围", question: "选择本次操作范围", options: [{ label: "当前目录", description: "仅处理当前目录" }] }] }
  assert.equal(parseConversation({ ...context, executions: [{ ...execution, pending_requests: [input] }] }).executions[0].pending_requests[0].questions?.[0].options?.[0].label, "当前目录")
  assert.throws(() => parseConversation({ ...context, executions: [{ ...execution, pending_requests: [{ ...input, questions: [...input.questions, ...input.questions] }] }] }))
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

test("workspace persists exact bot creation, routed messages and execution approvals before submission", async (t) => {
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
  const execution = { operation_id: "execution", target_node: "executor", session_id: "session", revision: 4, title: "工作", state: "running", session_state: "running", capabilities: ["respond_request"], pending_requests: [], freshness: "local_ledger", detail: "", blobs: [], goal: { task_id: "long-work", title: "工作", acceptance: "完成检查", status: "working", followup_mode: "continue", followup_version: 6, followup_scheduler_node: "original-scheduler" } }
  let scheduler: Record<string, unknown> | null = null
  const deferred = () => {
    let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
    const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
    return { promise, resolve, reject }
  }
  let conversationHold: ReturnType<typeof deferred> | null = null, messageHold: ReturnType<typeof deferred> | null = null
  const reconcileReplies: unknown[] = [], queries: Record<string, unknown>[] = [], acknowledgements: string[] = []
  let unreadNotifications: { notification_id: string; kind: string; text: string; conversation_id: string }[] = []
  const messages: Record<string, unknown>[] = [], controls: Record<string, unknown>[] = [], creations: Record<string, unknown>[] = [], followups: Record<string, unknown>[] = []
  let failControl = true, failBotCheck = false
  let createMode: "unknown" | "rejected" | "success" = "unknown"
  let knownBots: ReturnType<typeof parseBot>[] = [bot]
  const connection: ReadyConnection = {
    generation: 1, entry: { node_id: "gateway", address: "https://node.example", relay: false },
    principal: { network_id: "network", subject_id: "client", epoch: 1, can_manage: true },
    info: { node_id: "gateway", public_key: "key", version: "test", roles: ["gateway"], initialized: true },
    isCurrent: () => activeConnection === connection, subscribe: () => () => {}, request: async () => { throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: [] }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: unreadNotifications }
      if (method === "ack") { acknowledgements.push(String(body.notification_id)); unreadNotifications = unreadNotifications.filter(item => item.notification_id !== body.notification_id); return {} }
      if (method === "conversation") return conversationHold ? conversationHold.promise : { ...context, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, executions: [execution] }
      if (method === "execution_status") return {}
      if (method === "reconcile") {
        queries.push(JSON.parse(JSON.stringify(body)))
        const reply = reconcileReplies.length ? reconcileReplies.shift() : { operation: null, state: "not_found", target_node: body.target_node, checked_at_ms: 100 }
        if (reply instanceof Error) throw reply
        return reply
      }
      if (method === "create_bot") {
        creations.push(JSON.parse(JSON.stringify(body)))
        assert.deepEqual(JSON.parse(JSON.stringify(parseCreateBot(JSON.parse(stored.get("vibeface.pending-bot.client.network") ?? "null")))), creations.at(-1))
        if (createMode === "unknown") throw new Error("connection lost")
        if (createMode === "rejected") throw Object.assign(new Error("project unavailable"), { responded: true })
        const created = parseBot({ ...body, event_hash: "new-bot-event" })
        knownBots = [...knownBots.filter((item) => item.bot_id !== created.bot_id), created]
        return { bot: created }
      }
      if (method === "set_followup") {
        followups.push(JSON.parse(JSON.stringify(body)))
        execution.goal.followup_mode = "off"; execution.goal.followup_version += 1
        return { policy: { mode: "off", version: execution.goal.followup_version } }
      }
      if (method === "execution_control") {
        controls.push(JSON.parse(JSON.stringify(body)))
        const pending = parsePending(JSON.parse(stored.get("vibeface.pending.client.network") ?? "null"))
        assert.deepEqual(JSON.parse(JSON.stringify(pending[0].execution_control)), controls.at(-1))
        if (failControl) throw new Error("connection lost")
        return { operation: { state: "accepted", request: { operation_id: body.operation_id, target_node: "executor", conversation_id: "chat", session_id: "session" } } }
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
    await state.sendMessage("work", true)
    state = render()
    assert.ok(state.messagePending)
    const pending = JSON.parse(JSON.stringify(state.messagePending))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, text: "work", basis_heads: ["head"], scheduler_node: null, entry_node: "gateway", report_results: true })
    assert.equal(pending.scheduler_node, "chosen")
    await state.sendMessage("work")
    assert.deepEqual(messages[1], { ...messages[0], scheduler_node: "chosen" })
    await assert.rejects(render().sendMessage("different work"), /原消息/)
    connection.entry.node_id = "other-gateway"
    await assert.rejects(render().sendMessage("work"), /原入口/)
    connection.entry.node_id = "gateway"
    await assert.rejects(state.executionControl("execution", { type: "respond_request", request_id: 7, response: { decision: "decline" } }), /connection lost/)
    state = render()
    assert.equal(state.pending.length, 1)
    assert.deepEqual(controls[0], { conversation_id: "chat", operation_id: state.pending[0].operation_id, expected_operation_id: "execution", expected_revision: 4, action: { type: "respond_request", request_id: 7, response: { decision: "decline" } } })
    await assert.rejects(state.executionControl("execution", { type: "interrupt" }), /原操作待核对/)
    execution.revision = 9
    failControl = false
    await state.retryExecutionControl(state.pending[0])
    assert.deepEqual(controls[1], controls[0])
    assert.equal(render().pending.length, 0)
    assert.deepEqual(queries[0], { target_node: "executor", operation_id: controls[0].operation_id })
    await t.test("control retries query the original identity and never resend unknown or already completed writes", async () => {
      failControl = true
      await assert.rejects(render().executionControl("execution", { type: "interrupt" }), /connection lost/)
      const original = render().pending[0]
      const originalRequest = { operation_id: original.operation_id, target_node: original.target_node, conversation_id: original.conversation_id, session_id: original.session_id }
      const before = controls.length
      for (const reply of [
        { operation: { request: originalRequest, state: "running" } }, { operation: { request: originalRequest, state: "unknown" } },
        new Error("original target unreachable"),
        ...[{ operation_id: "wrong" }, { target_node: "wrong" }, { conversation_id: "wrong" }, { session_id: "wrong" }].map(wrong => ({ operation: { request: { ...originalRequest, ...wrong }, state: "succeeded" } })),
        { operation: null, state: "not_found", target_node: "wrong", checked_at_ms: 100 },
        { operation: null, state: "not_found", target_node: original.target_node },
      ]) {
        reconcileReplies.push(reply)
        await assert.rejects(render().retryExecutionControl(original))
        assert.equal(controls.length, before)
        assert.equal(render().pending[0]?.operation_id, original.operation_id)
        assert.deepEqual(queries.at(-1), { target_node: original.target_node, operation_id: original.operation_id })
      }
      reconcileReplies.push({ operation: { request: originalRequest, state: "succeeded" } })
      await render().retryExecutionControl(original)
      assert.equal(controls.length, before)
      assert.equal(render().pending.length, 0)
      failControl = false
    })
    await render().stopFollowup("execution")
    assert.deepEqual(followups, [{ conversation_id: "chat", task_id: "long-work", mode: "off", expected_version: 6, scheduler_node: "original-scheduler" }])
    assert.equal(render().data?.executions[0].goal?.followup_mode, "off")
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
        if (outcome === "success") reading.resolve({ ...context, scheduler: null, executions: [execution], missing: { events: ["parent"], blobs: [], conflicts: [] } })
        else reading.reject(new Error("new read unavailable"))
        await new Promise(setImmediate)
        assert.equal(render().writable, outcome === "success")
        assert.equal(render().incomplete, true)
        assert.equal(acknowledgements.length, ackBefore)
        await assert.rejects(render().sendMessage("reconnect payload"), /补读|上下文完整/)
        const control = parsePending([{ operation_id: "original-control", target_node: "executor", conversation_id: "chat", session_id: "session", execution_control: { operation_id: "original-control", conversation_id: "chat", expected_operation_id: "execution", expected_revision: 9, action: { type: "interrupt" } } }])[0]
        reconcileReplies.push({ operation: { request: control, state: "unknown" } })
        assert.equal((await render().reconcile(control)).operation?.state, "unknown")
        assert.deepEqual(queries.at(-1), { target_node: "executor", operation_id: "original-control" })
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
      conversationHold.resolve({ ...context, scheduler: null, executions: [execution], missing: { events: ["unrelated-parent"], blobs: [], conflicts: [] } })
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
      conversationHold.resolve({ ...context, conversation: { ...context.conversation, conversation_id: currentId }, scheduler: null, executions: [] })
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

test("taskless execution projection and persisted approval actions retain nullable session identity", () => {
  const execution = { operation_id: "command", target_node: "executor", session_id: null, revision: 0, title: "查询状态", state: "succeeded", session_state: "none", capabilities: [], pending_requests: [], freshness: "local_ledger", detail: "退出码 0\nclean", blobs: [] }
  const payload = parseConversation({ ...context, executions: [execution], operations: [{ state: "succeeded", request: { ...request, session_id: null } }] })
  assert.deepEqual(JSON.parse(JSON.stringify(payload.executions[0])), execution)
  assert.equal(payload.operations[0].request.session_id, null)
  assert.throws(() => parseConversation({ ...context, executions: [{ ...execution, state: "idle" }] }))
  const execution_control = { conversation_id: "chat", operation_id: "approval", expected_operation_id: "execution", expected_revision: 4, action: { type: "respond_request", request_id: 1, response: { decision: "decline" } } }
  const pending = { operation_id: "approval", target_node: "executor", conversation_id: "chat", session_id: "session", execution_control }
  assert.deepEqual(parsePending([pending])[0], pending)
  assert.throws(() => parsePending([{ ...pending, execution_control: { ...execution_control, conversation_id: "other" } }]))
  assert.throws(() => parsePending([{ ...pending, execution_control: { ...execution_control, action: { type: "respond_request", request_id: 1 } } }]))
})

test("future processing can be revoked while the current execution is unknown and offline", async () => {
  const elements: { type: unknown; props: Record<string, unknown> }[] = [], stopped: string[] = []
  const jsx = (type: unknown, props: Record<string, unknown>) => { const element = { type, props }; elements.push(element); return element }
  const ui = new Proxy({}, { get: (_target, name) => String(name) })
  const module: { exports: Record<string, unknown> } = { exports: {} }
  const require = (specifier: string) => {
    if (specifier === "react") return { useId: () => "work", useState: (initial: unknown) => [initial, () => {}] }
    if (specifier === "react/jsx-runtime") return { jsx, jsxs: jsx }
    if (specifier === "../identity/context") return { useDevice: () => ({ directory: { members: [{ subject_id: "executor", name: "工作电脑" }] } }) }
    if (specifier === "./context") return { useWorkspace: () => ({ pending: [], nodes: [], writable: true, selectedBot: null, connectionOnline: true, run: (action: () => Promise<void>) => action(), stopFollowup: async (operationId: string) => { stopped.push(operationId) } }) }
    if (specifier === "./labels") return { nodeName: () => "device", stateLabel: () => "unknown" }
    return ui
  }
  const filename = fileURLToPath(new URL("./execution-activity.tsx", import.meta.url))
  const compiled = transpileModule(readFileSync(filename, "utf8"), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
  new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext()(module.exports, require, module)
  const ExecutionActivity = module.exports.ExecutionActivity
  assert.ok(typeof ExecutionActivity === "function")
  const execution = { operation_id: "execution", target_node: "executor", session_id: "session", revision: 4, title: "work", state: "unknown", session_state: "unknown", capabilities: ["interrupt"], pending_requests: [], freshness: "unknown", detail: "", blobs: [], goal: { task_id: "work-goal", title: "goal", acceptance: "done", status: "working", followup_mode: "continue", followup_version: 3 } }
  ExecutionActivity({ execution, conversationId: "chat" })
  const stop = elements.find(({ props }) => props.children === "取消后续任务")
  assert.ok(stop && typeof stop.props.action === "function")
  assert.equal(stop.props.disabled, false)
  assert.equal(elements.find(({ props }) => props.children === "停止当前执行")?.props.disabled, true)
  await stop.props.action()
  assert.deepEqual(stopped, ["execution"])
  elements.length = 0
  ExecutionActivity({ execution: { ...execution, goal: { ...execution.goal, followup_mode: "off" } }, conversationId: "chat" })
  assert.equal(elements.some(({ props }) => props.children === "取消后续任务"), false)
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
    chatSlot: { conversationId: 'chat', missing: false }, nodes: [], liveProgress: [], pending: [], writable: true, selectedBot: { name: 'bot' }, connectionOnline: true,
    info: { node_id: 'gateway' }, sendMessage: () => work.promise, run: async (action: () => Promise<void>) => { try { await action() } catch {} },
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

test("pending-operation results are shown only for the connection that checked the original ID", async () => {
  let generation = 1, work = pendingResult()
  const operation = { operation_id: 'original', target_node: 'executor', conversation_id: 'chat', session_id: 'session' }
  const workspace = () => { const owner = generation; return { generation: owner, isCurrent: () => owner === generation, nodes: [{ node_id: 'executor', online: true, revoked: false }], connectionOnline: true,
    reconcile: () => work.promise, run: (action: () => Promise<void>) => action() } }
  let currentWorkspace = workspace()
  const app = componentHarness('./execution-activity.tsx', { './context': { useWorkspace: () => currentWorkspace }, './reconcile': { reconcileDecision }, './labels': { stateLabel: (state: string) => state } })
  const render = () => app.render('PendingExecutionControl', { operation })
  const check = () => render().find(({ props }) => props.children === '核对原操作')?.props.action?.()
  try {
    const checking = check(); work.resolve({ operation: null, state: 'not_found', target_node: 'executor', checked_at_ms: 100 }); await checking
    assert.equal(render().some(({ props }) => props.children === '确认原请求结束并解除待核对'), true)
    work = pendingResult(); const old = check()
    generation++; currentWorkspace = workspace()
    assert.equal(render().some(({ props }) => props.role === 'status'), false)
    work.resolve({ operation: { request: operation, state: 'succeeded' } }); await old
    assert.equal(render().some(({ props }) => props.role === 'status'), false)
  } finally { app.close() }
})

test("connection form keeps current errors and busy ownership when an earlier connection is cancelled", async () => {
  let generation = 1, work = pendingResult(), synchronousFailure = false
  const device = {
    connectionGeneration: () => generation, entries: [{ address: 'https://node.example', node_id: 'target', relay: true }], initialInvitation: '',
    identity: { nodeId: 'browser' }, phase: 'disconnected', error: '',
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
  const fields = new Map(['bundle', 'password', 'old_password', 'confirmation', 'name', 'bootstrap', 'address', 'node_id', 'relay'].map(name => [name, new Input()]))
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
  const ready = () => { const owner = generation; return { isCurrent: () => owner === generation, principal: { can_manage: true, network_id: 'network' }, entry: { address: 'https://node.example', node_id: 'target', relay: true } } }
  const device = { connectionGeneration: () => generation, ready: ready(), identity: null, entries: [], directory: null, pairs: [], refreshDirectory: async () => {}, createInvitation: () => creation.promise }
  const app = componentHarness('../identity/device-controls.tsx', { './context': { useDevice: () => device } }, { navigator: { clipboard: { writeText: () => clipboard.promise } } })
  const render = () => app.render('DeviceControls')
  const generate = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {} })
  const button = () => render().find(({ props }) => props.children === '生成邀请')?.props
  try {
    render(); render(); generate(); creation.resolve('test-only-invitation'); await settleComponent()
    const copy = render().find(({ props }) => props.children === '复制邀请')
    assert.ok(copy?.props.onClick); copy.props.onClick()
    assert.equal(button()?.disabled, true)
    generation++; device.ready = ready(); render(); render()
    assert.equal(button()?.disabled, false)
    creation = pendingResult(); generate()
    clipboard.resolve(undefined); await settleComponent()
    assert.equal(button()?.disabled, true)
    assert.equal(render().some(({ props }) => props.children === '邀请已复制。'), false)
    creation.resolve('new-test-only-invitation'); await settleComponent()
    assert.equal(button()?.disabled, false)
    assert.equal(render().some(({ props }) => props.children === '邀请已生成，请通过可信渠道发送。'), true)
  } finally { app.close() }
})
