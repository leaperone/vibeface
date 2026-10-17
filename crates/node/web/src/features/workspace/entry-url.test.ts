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
import { hasQueuedMessage, inputText, parseArtifactPage, parseConversation, parseNodes, parsePending, parsePendingTasks } from "./model.ts"
import { parseReconcile, reconcileDecision } from "./reconcile.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test" }, messages: [], heads: ["head"],
  missing: { events: [], blobs: [], conflicts: [] }, tasks: [], sessions: [], operations: [],
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
test("stored operations retain request contents and enforce the personal network", () => {
  const full = { ...request, network_id: "network", task_id: null, expected_revision: 3, basis_heads: ["head"], resources: [], action: { type: "send", text: "original" } }
  const pending = { ...request, request: full }
  assert.deepEqual(parsePending([pending], "network")[0]?.request, full)
  assert.throws(() => parsePending([pending], "other-network"))
  assert.throws(() => parsePending([{ ...pending, target_node: "other" }], "network"))
})
test("manual task retries keep the original task identity without replaying legacy followup authorization", () => {
  const original = { conversation_id: "chat", task_id: "task", title: "title", goal: "goal", acceptance: "done" }
  assert.deepEqual(parsePendingTasks([original]), [original])
  assert.deepEqual(parsePendingTasks([{ ...original, auto_followup: true }]), [original])
  assert.deepEqual(parsePendingTasks([{ ...original, auto_followup: false }]), [original])
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
test("stored task controls retain exact approval identity and expected execution revision", () => {
  const control = { ...request, network_id: "network", task_id: "task", expected_operation_id: "original-execution", expected_revision: 4, action: "respond_request", request_id: "approval", response: { decision: "decline" } }
  const pending = { ...request, control }
  assert.deepEqual(parsePending([pending], "network")[0]?.control, control)
  assert.throws(() => parsePending([pending], "other-network"))
  assert.throws(() => parsePending([{ ...pending, session_id: "other-session" }], "network"))
  assert.throws(() => parsePending([{ ...pending, control: { ...control, expected_revision: -1 } }], "network"))
  assert.throws(() => parsePending([{ ...pending, control: { ...control, response: undefined, request_id: {} } }], "network"))
})
test("queue, task authorization, requests and result references parse as explicit facts", () => {
  const task = { task_id: "task", title: "work", goal: "goal", acceptance: "done", detail: "pending", status: "working", followup_mode: "report", followup_version: 3, blobs: [{ hash: "execution-result", size: 20 }], binding: { target_node: "executor", session_id: "session", operation_id: "op", revision: 4, state: "running", capabilities: ["read", "respond_request"], pending_requests: [{ request_id: 1, kind: "approval", text: "write file", choices: [{ label: "拒绝", response: { decision: "decline" } }] }] } }
  const scheduler = { jobs: [{ trigger: "message:one", state: "queued", kind: "message", sequence: 0, message_id: "one", read_only: true }], capabilities: { streaming: true, cancel: false, steer: false }, write_frozen: true }
  const message = { message_id: "one", event_hash: "event", role: "assistant", text: "summary", created_at_ms: 1, blobs: [{ hash: "result", size: 20 }] }
  const parsed = parseConversation({ ...context, tasks: [task], scheduler, messages: [message] })
  assert.deepEqual(parsed.tasks[0].binding?.pending_requests[0].choices?.[0].response, { decision: "decline" })
  assert.equal(parsed.tasks[0].followup_version, 3)
  assert.deepEqual(parsed.tasks[0].blobs, [{ hash: "execution-result", size: 20 }])
  assert.equal(parsed.scheduler?.jobs[0].read_only, true)
  assert.deepEqual(parsed.messages[0].blobs, [{ hash: "result", size: 20 }])
  assert.equal(parseConversation(context).scheduler, null)
  assert.throws(() => parseConversation({ ...context, scheduler: { ...scheduler, jobs: [{ ...scheduler.jobs[0], sequence: -1 }] } }))
  assert.throws(() => parseConversation({ ...context, tasks: [{ ...task, followup_mode: "approve_everything" }] }))
  assert.throws(() => parseConversation({ ...context, tasks: [{ ...task, binding: { ...task.binding, capabilities: undefined } }] }))
  assert.equal(parseConversation({ ...context, tasks: [{ ...task, binding: { ...task.binding, freshness: "cached", checked_at_ms: 100 } }] }).tasks[0].binding?.freshness, "cached")
  assert.throws(() => parseConversation({ ...context, tasks: [{ ...task, binding: { ...task.binding, freshness: "live_because_online" } }] }))
  const input = { request_id: "input", kind: "input", text: "请补充需求", questions: [{ id: "scope", header: "范围", question: "选择本次操作范围", options: [{ label: "当前目录", description: "仅处理当前目录" }] }] }
  const inputTask = { ...task, binding: { ...task.binding, pending_requests: [input] } }
  assert.equal(parseConversation({ ...context, tasks: [inputTask] }).tasks[0].binding?.pending_requests[0].questions?.[0].options?.[0].label, "当前目录")
  assert.throws(() => parseConversation({ ...context, tasks: [{ ...inputTask, binding: { ...inputTask.binding, pending_requests: [{ ...input, questions: [...input.questions, ...input.questions] }] } }] }))
})
test("artifact pages stay bound to their original reference and advance without skipping the end", () => {
  const page = { blob_hash: "result", text: "first", offset: 0, next_offset: 5, size: 10 }
  assert.deepEqual(parseArtifactPage(page, "result", 0), { text: "first", offset: 0, next_offset: 5, size: 10 })
  assert.equal(parseArtifactPage({ ...page, offset: 5, next_offset: null }, "result", 5).next_offset, null)
  for (const invalid of [{ ...page, blob_hash: "other" }, { ...page, offset: 1 }, { ...page, next_offset: 0 }, { ...page, next_offset: 11 }, { ...page, next_offset: undefined }, { ...page, text: "" }, { ...page, next_offset: null }, { ...page, next_offset: 5, text: "x" }, { ...page, size: 0, next_offset: null }]) assert.throws(() => parseArtifactPage(invalid, "result", 0))
  assert.deepEqual(parseArtifactPage({ blob_hash: "result", text: "中", offset: 0, next_offset: null, size: 3 }, "result", 0), { text: "中", offset: 0, next_offset: null, size: 3 })
})

test("creation fields use the server UTF-8 byte limits and reject whitespace", () => {
  assert.equal(inputText("  名称  ", "会话名称", 512), "名称")
  assert.equal(inputText("中".repeat(170), "会话名称", 512), "中".repeat(170))
  assert.throws(() => inputText("中".repeat(171), "会话名称", 512), /512/)
  assert.throws(() => inputText("   ", "工单目标", 65536), /不能为空/)
  assert.equal(inputText("a".repeat(65536), "验收条件", 65536).length, 65536)
  assert.throws(() => inputText("中".repeat(21846), "验收条件", 65536), /65536/)
})

test("workspace callers retain offline message identity and bound execution tasks", async () => {
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
    history: { replaceState() {} }, setTimeout: () => 1, clearTimeout() {}, setInterval: () => 1, clearInterval() {},
  }
  const sandbox = createContext({
    window: browser, URL, TextEncoder, crypto: globalThis.crypto,
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
  const configured = { manager: "cursor_agent", executors: [], roots: [] }
  const node = (node_id: string, changes: Record<string, unknown> = {}) => ({ node_id, local: false, online: true, revoked: false, roles: ["scheduler"], capabilities: configured, ...changes })
  let rawNodes = [node("revoked", { revoked: true }), node("executor", { roles: ["executor"] }), node("unconfigured", { capabilities: { ...configured, manager: null } }), node("offline", { online: false, capabilities: null }), node("other")]
  let scheduler: Record<string, unknown> | null = null
  const session = { target_node: "executor", session_id: "session", executor: "cursor_agent", state: "idle", cwd: "/project", revision: 4 }
  const task = { task_id: "task", title: "work", goal: "goal", acceptance: "done", detail: "ready", status: "working", binding: { target_node: session.target_node, session_id: session.session_id, operation_id: "execution", revision: session.revision, state: "idle", capabilities: ["send"], pending_requests: [] } }
  const messages: Record<string, unknown>[] = [], operations: Record<string, unknown>[] = []
  const connection: ReadyConnection = {
    generation: 1, entry: { node_id: "gateway", address: "https://node.example", relay: false },
    principal: { network_id: "network", subject_id: "client", epoch: 1, can_manage: true },
    info: { node_id: "gateway", public_key: "key", version: "test", roles: ["gateway"], initialized: true },
    isCurrent: () => true, subscribe: () => () => {}, request: async () => { throw new Error("unexpected request") },
    async rpc(method, body = {}) {
      if (method === "nodes") return { nodes: rawNodes }
      if (method === "conversations") return { conversations: [context.conversation] }
      if (method === "notifications") return { notifications: [] }
      if (method === "conversation") return { ...context, tasks: [task], sessions: [session], scheduler }
      if (method === "operate") {
        operations.push(JSON.parse(JSON.stringify(body.request)))
        return { operation: { state: "accepted", request: body.request } }
      }
      assert.equal(method, "message")
      messages.push(JSON.parse(JSON.stringify(body)))
      scheduler = { jobs: [{ trigger: `message:${body.message_id}`, kind: "human", state: "pending_delivery", sequence: 0, message_id: body.message_id, target_node: body.scheduler_node }] }
      return { accepted: true, queued: false, message_id: body.message_id, scheduler_node: body.scheduler_node }
    },
  }
  const render = () => {
    cursor = 0
    const state = useWorkspaceState(connection)
    afterRender.splice(0).forEach((run) => run())
    return state
  }
  try {
    render(); render()
    await new Promise(setImmediate)
    let state = render()
    assert.equal(state.writable, true)
    assert.equal(state.schedulerNodeId, "offline")
    await state.refresh()
    state = render()
    assert.equal(state.schedulerNodeId, "offline")
    assert.equal(state.chatSlot.schedulerNodeId, "offline")
    assert.equal(state.chatSlot.schedulerOnline, false)
    assert.equal(state.chatSlot.schedulerAllowed, true)
    await state.sendMessage("work")
    state = render()
    assert.ok(state.messagePending)
    const pending = JSON.parse(JSON.stringify(state.messagePending))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, text: "work", basis_heads: ["head"], scheduler_node: "offline", entry_node: "gateway", report_results: false })
    assert.equal(pending.scheduler_node, "offline")
    assert.equal(pending.entry_node, "gateway")
    assert.deepEqual(JSON.parse(stored.get("vibeface.pending-messages.client.network") ?? "null"), [pending])
    state.setSchedulerNodeId("other")
    state = render()
    await state.refresh()
    state = render()
    assert.equal(state.schedulerNodeId, "other")
    assert.equal(state.chatSlot.schedulerNodeId, "offline")
    await state.sendMessage("work")
    state = render()
    assert.deepEqual(messages, [messages[0], messages[0]])
    assert.deepEqual(JSON.parse(JSON.stringify(state.messagePending)), pending)
    await assert.rejects(state.sendMessage("different work"), /原消息/)
    connection.entry.node_id = "other-gateway"
    await assert.rejects(state.sendMessage("work"), /原入口/)
    connection.entry.node_id = "gateway"
    for (const unavailable of [{ revoked: true }, { roles: ["executor"] }, { capabilities: { ...configured, manager: null } }]) {
      rawNodes = [node("offline", unavailable), node("other")]
      await state.refresh()
      state = render()
      assert.equal(state.chatSlot.schedulerAllowed, false)
      await assert.rejects(state.sendMessage("work"), /授权|配置/)
    }
    assert.equal(messages.length, 2)
    assert.deepEqual(JSON.parse(stored.get("vibeface.pending-messages.client.network") ?? "null"), [pending])
    await state.operate(session, { type: "send", text: "next" }, "chat")
    assert.equal(operations.length, 1)
    assert.equal(typeof operations[0].operation_id, "string")
    assert.deepEqual(operations[0], { operation_id: operations[0].operation_id, network_id: "network", conversation_id: "chat", task_id: "task", target_node: "executor", session_id: "session", expected_revision: 4, basis_heads: ["head"], resources: [], action: { type: "send", text: "next" } })
    assert.equal(render().pending.length, 0)
  } finally {
    for (const effect of effects.values()) effect.cleanup?.()
  }
})
