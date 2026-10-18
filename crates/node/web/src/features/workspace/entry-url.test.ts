import assert from "node:assert/strict"
import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import { readFileSync } from "node:fs"
import { dirname, extname, resolve } from "node:path"
import test from "node:test"
import { readFile } from "node:fs/promises"
import { stripTypeScriptTypes } from "node:module"
import { readStorage, writeStorage } from "./storage.ts"
import { parseRemoteExecutionControl } from "../../lib/remote-execution.ts"
import { verifyPreparedBody } from "../../lib/rpc-prepare.ts"
import { prepareRpc, rpcAction } from "../../lib/rpc-action.ts"
import { parseIdentityScope, projectScopeKey, scopeKey } from "../../lib/scope.ts"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis } from "./labels.ts"
import { inputText, parseConversation, parseNodes, parsePending, parsePendingBindings, parsePendingTasks, parsePendingControls, parseBot, parseCreateBot, hasQueuedMessage } from "./model.ts"
import { parseReconcile, reconcileDecision } from "./reconcile.ts"
import type { OperationRequest } from "./types.ts"
import { executionWorkIntent } from '../../lib/execution-work.ts'
import { parseOperation, parseOperationRequest } from './model.ts'

const context = {
  conversation: { conversation_id: "chat", title: "test" }, messages: [], heads: ["head"],
  missing: { events: [], blobs: [], conflicts: [] }, tasks: [], sessions: [], executions: [], operations: [], scheduler: null,
}
const request: OperationRequest = {
  space_id: "space", operation_id: "op", target_node: "executor", conversation_id: "chat", session_id: "session", task_id: "task",
  binding: { binding: { space_id: "space", conversation_id: "chat", task_id: "task", task_ref: "a".repeat(64), authority_node: "b".repeat(64), generation: 1, target_node: "executor", session_id: "session", open_spec: { type: "open", executor: "codex", cwd: "/project" }, previous_binding_hash: null, previous_seal: null }, signature: "c".repeat(128) },
  expected_revision: 3, basis_heads: ["head"], resources: ["/project"], action: { type: "send", text: "original" },
}
test("conversation navigation stays in the trusted web and carries no credentials", () => {
  assert.equal(conversationHref("https://node.example/app?capability=secret&basis=%5B%5D#token", "next", { account_id: "a", space_id: "s", device_id: "d" }, "project"), "/app?conversation=next&account=a&space=s&device=d&project=project")
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
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "space", target_node: "executor", checked_at_ms: 100 }), request), "missing")
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "space", target_node: "other", checked_at_ms: 100 }), request), "pending")
  assert.throws(() => parseReconcile({ operation: null, state: "not_found", space_id: "space", target_node: "executor" }))
  assert.throws(() => reconcileDecision(parseReconcile({ operation: { ...operation, request: { ...request, operation_id: "other" } } }), request))
  assert.throws(() => reconcileDecision(parseReconcile({ operation: { ...operation, request: { ...request, space_id: "other-space" } } }), request))
  assert.throws(() => parseReconcile({ operation: { ...operation, request: { space_id: "space", operation_id: "op", target_node: "executor", conversation_id: "chat", session_id: "session" } } }))
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "other-space", target_node: "executor", checked_at_ms: 100 }), request), "pending")
})
test("stored operations retain request contents and enforce the space", () => {
  const binding = { binding: { space_id: "space", conversation_id: "chat", task_id: "task", task_ref: "a".repeat(64), authority_node: "b".repeat(64), generation: 1, target_node: "executor", session_id: "session", open_spec: { type: "open", executor: "codex", cwd: "/project" }, previous_binding_hash: null, previous_seal: null }, signature: "c".repeat(128) }
  const full = { ...request, space_id: "space", task_id: "task", binding, expected_revision: 3, basis_heads: ["head"], resources: [], action: { type: "send", text: "original" } }
  const pending = { ...request, request: full }
  assert.deepEqual(parsePending([pending], "space")[0]?.request, full)
  assert.throws(() => parsePending([pending], "other-space"))
  assert.throws(() => parsePending([{ ...pending, target_node: "other" }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, space_id: undefined, network_id: "space" } }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, task_id: null } }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, binding: undefined } }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, binding: { ...binding, binding: { ...binding.binding, task_id: "other" } } } }], "space"))
})

test('ordinary operations preserve complete requests and require dispatch evidence', () => {
  const { binding: _binding, ...fields } = request
  const conversation = { ...fields, task_id: null, binding: null, manager_authority: { dispatch: 'original-manager' } }
  assert.deepEqual(parseOperationRequest(conversation, 'space'), conversation)
  const command = { ...conversation, session_id: null, expected_revision: 0, manager_authority: null, action: { type: 'command', command: 'pwd', cwd: '/project', timeout_secs: 10 } }
  assert.deepEqual(parseOperationRequest(command, 'space'), command)
  assert.throws(() => parseOperation({ request: command, state: 'running' }), /派单证明/)
  const proof = { project_dispatch: { hash: 'dispatch' }, project_execution_origin: { kind: 'command' } }
  assert.deepEqual(parseOperation({ request: command, state: 'running', ...proof }).request, command)
  assert.throws(() => parseOperationRequest({ ...command, binding: request.binding }, 'space'), /绑定/)
  assert.throws(() => parseOperationRequest({ ...command, expected_revision: 1 }, 'space'), /版本/)
  assert.throws(() => parseOperationRequest(command, 'other-space'), /空间/)
  const pending = { ...command, request: command, attempt: { body: { method: 'operate', request: command }, invocation: { nonce: 'original' } } }
  assert.deepEqual(parsePending([pending], 'space')[0].request, command)
})

test('conversation work carries the accepted open while commands have no session', () => {
  const open = { operation_id: 'opened', invocation_hash: 'invocation', admission_hash: 'admission' }
  const session = { kind: 'conversation_session' as const, conversation: 'chat', execution_session: { target: 'device', session_id: 'session' }, executor: 'codex', canonical_cwd: '/project', open }
  assert.deepEqual(executionWorkIntent(session), { ...session, open: { kind: 'existing', open } })
  const command = { kind: 'command' as const, request_hash: 'request', canonical_cwd: '/project' }
  assert.deepEqual(executionWorkIntent(command), command)
  assert.equal('execution_session' in executionWorkIntent(command), false)
})

test("pending task signatures retain every bounded local attempt and reject old records", () => {
  const body = { method: "create_task", conversation_id: "chat", task_id: "task", title: "title", goal: "goal", acceptance: "acceptance", auto_followup: false }
  const first = { body: { ...body, authorization_id: "task:task" }, invocation: { nonce: "first" } }
  const second = { body: first.body, invocation: { nonce: "second" } }
  const pending = { ...body, attempts: [first, second] }
  assert.deepEqual(parsePendingTasks([pending])[0]?.attempts, [first, second])
  assert.throws(() => parsePendingTasks([{ ...pending, attempts: undefined }]))
  assert.throws(() => parsePendingTasks([{ ...pending, attempts: Array(9).fill(first) }]))
  assert.throws(() => parsePendingTasks([{ ...pending, attempts: [{ body, invocation: null }] }]))
})

test("unconfirmed bindings keep the original open intent and signed request", () => {
  const task = { task_id: "task", title: "title", goal: "goal", acceptance: "acceptance", detail: "", status: "open", task_creation: { hash: "creation" }, authority_public_key: "key", binding: null }
  const intent = { space_id: request.space_id, operation_id: request.operation_id, target_node: request.target_node, conversation_id: request.conversation_id, session_id: request.session_id, task_id: "task", expected_revision: 0, basis_heads: ["head"], resources: ["/project"], action: { type: "open", executor: "codex", cwd: "/project" } }
  const pending = { request: intent, task, attempt: { body: { session_id: request.session_id }, invocation: { nonce: "attempt" } } }
  assert.deepEqual(parsePendingBindings([pending], "space"), [pending])
  assert.throws(() => parsePendingBindings([pending], "other-space"))
  assert.throws(() => parsePendingBindings([{ ...pending, request: { ...intent, task_id: "other" } }], "space"))
  assert.throws(() => parsePendingBindings([{ ...pending, request: { ...intent, action: { type: "send", text: "do not replay" } } }], "space"))
})

test("creation fields use the server UTF-8 byte limits and reject whitespace", () => {
  assert.equal(inputText("  名称  ", "机器人名称", 512), "名称")
  assert.equal(inputText("中".repeat(170), "机器人名称", 512), "中".repeat(170))
  assert.throws(() => inputText("中".repeat(171), "机器人名称", 512), /512/)
  assert.throws(() => inputText("   ", "职责说明", 8192), /不能为空/)
  assert.equal(inputText("a".repeat(8192), "职责说明", 8192).length, 8192)
  assert.throws(() => inputText("中".repeat(2731), "职责说明", 8192), /8192/)
})

test("workspace persists exact bot creation, routed messages and execution approvals before submission", async () => {
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
    window: browser, URL, TextEncoder, TextDecoder, Error, structuredClone, crypto: globalThis.crypto,
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
      if (specifier.endsWith("vibeface_secure.js")) return { nodeFingerprint: (key: string) => key }
      if (specifier.endsWith("task-proof")) return { verifyTaskCreation: () => ({ taskRef: "a".repeat(64) }), verifyExecutionBinding: (_connection: unknown, _task: unknown, _request: unknown, _open: unknown, proof: unknown) => proof }
      const path = specifier.startsWith("@/") ? resolve(sourceRoot, specifier.slice(2)) : resolve(dirname(filename), specifier)
      return load(extname(path) ? path : `${path}.ts`)
    }
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(module.exports, require, module)
    return module.exports
  }
  type Workspace = ReturnType<typeof import("./context.tsx").useWorkspace>
  const workspaceModule = load(fileURLToPath(new URL("./context.tsx", import.meta.url)))
  assert.equal(typeof workspaceModule.useWorkspaceState, "function")
  const { useWorkspaceState } = workspaceModule as { useWorkspaceState(connection: unknown): Workspace }
  const bot = { bot_id: "bot", conversation_id: "chat", name: "助手", instructions: "处理项目", event_hash: "bot-event" }
  const localRequest = { ...request, operation_id: "execution", target_node: "project-node", binding: { ...request.binding, binding: { ...request.binding.binding as Record<string, unknown>, target_node: "project-node" } } }
  const localTask = { task_id: "task", title: "work", goal: "done", acceptance: "done", detail: "", status: "working", task_creation: { hash: "created" }, authority_public_key: "project-node", binding: localRequest.binding }
  const localSession = { task_id: "task", target_node: "project-node", session_id: "session", executor: "codex", state: "running", cwd: "/project", revision: 4, native_session_id: "native", task_creation: localTask.task_creation, authority_public_key: "project-node", binding: localRequest.binding }
  const localOperation = { state: "running", request: localRequest }
  const liveStatus = { session: localSession, operation: localOperation, turn: { turn: { id: "turn" } }, snapshot: { threads: { native: { last_turn: { id: "turn", status: "inProgress" } } }, pending_requests: [{ id: 7, params: { threadId: "native", turnId: "turn" } }] } }
  const execution = { operation_id: "execution", target_node: "project-node", session_id: "session", revision: 4, title: "工作", state: "running", session_state: "running", capabilities: ["respond_request"], pending_requests: [], freshness: "local_ledger", detail: "", blobs: [], goal: { task_id: "long-work", title: "工作", acceptance: "完成检查", status: "working", followup_mode: "continue", followup_version: 6, followup_scheduler_node: "original-scheduler" } }
  let scheduler: Record<string, unknown> | null = null
  const managerControls: Record<string, unknown>[] = []
  const messages: Record<string, unknown>[] = [], controls: Record<string, unknown>[] = [], creations: Record<string, unknown>[] = [], followups: Record<string, unknown>[] = []
  let failControl = true, failBotCheck = false, failPrepare = false, returnedRoute = "chosen"
  let createMode: "unknown" | "rejected" | "success" = "unknown"
  let knownBots: ReturnType<typeof parseBot>[] = [bot]
  const connection = {
    generation: 1, entry: { node_id: "gateway", address: "https://node.example", relay: false },
    scope: { account_id: "account", space_id: "space", device_id: "client" },
    audience: { device: "project-node", service: "project-service" }, audiencePublicKey: "project-node",
    project: { id: "res2_" + "a".repeat(64), grant: "grant" },
    authorization: { actions: ["conversation_create", "message_append", "execution_respond_approval", "task_followup"], enrollmentRights: [] },
    info: { node_id: "gateway", public_key: "key", version: "test", roles: ["gateway"], initialized: true },
    isCurrent: () => true, subscribe: () => () => {}, request: async () => { throw new Error("unexpected request") },
    async rpc(method: string, body: Record<string, unknown> = {}) {
      if (method === "nodes") return { nodes: [] }
      if (method === "bots") { if (failBotCheck) throw new Error("lookup unavailable"); return { bots: knownBots } }
      if (method === "notifications") return { notifications: [] }
      if (method === "conversation") return { ...context, conversation: { ...context.conversation, conversation_id: body.conversation_id }, scheduler, ...(body.conversation_id === "chat" ? { executions: [execution], tasks: [localTask], sessions: [localSession], operations: [localOperation] } : { executions: [], tasks: [], sessions: [], operations: [] }) }
      if (method === "execution_status") return liveStatus
      if (method === "create_bot") {
        creations.push(JSON.parse(JSON.stringify(body)))
        assert.deepEqual(JSON.parse(JSON.stringify(parseCreateBot(JSON.parse(stored.get(`vibeface.${projectScopeKey(connection.scope, connection.project.id, "pending-bot")}`) ?? "null")))), creations.at(-1))
        if (createMode === "unknown") throw new Error("connection lost")
        if (createMode === "rejected") throw Object.assign(new Error("project unavailable"), { responded: true })
        const created = parseBot({ ...body, event_hash: "new-bot-event" })
        knownBots = [...knownBots.filter((item) => item.bot_id !== created.bot_id), created]
        return { bot: created }
      }
      if (method === "cancel_manager") {
        managerControls.push(structuredClone(body))
        return { cancel_requested: true, job: { state: 'cancelled' } }
      }
      if (method === "set_followup") {
        followups.push(JSON.parse(JSON.stringify(body)))
        execution.goal.followup_mode = "off"; execution.goal.followup_version += 1
        return { policy: { mode: "off", version: execution.goal.followup_version } }
      }
      if (method === "execution_control") {
        controls.push(JSON.parse(JSON.stringify(body)))
        const pending = parsePendingControls(JSON.parse(stored.get(`vibeface.${projectScopeKey(connection.scope, connection.project.id, "pending-controls")}`) ?? "null"))
        assert.deepEqual(JSON.parse(JSON.stringify(pending[0].attempt?.body)), controls.at(-1))
        if (failControl) throw new Error("connection lost")
        return { operation: { state: "succeeded", request: body.request } }
      }
      assert.equal(method, "message")
      messages.push(JSON.parse(JSON.stringify(body)))
      scheduler = { jobs: [{ trigger: `message:${body.message_id}`, kind: "human", state: "pending_delivery", sequence: 0, message_id: body.message_id, target_node: "chosen" }] }
      return { accepted: true, queued: false, message_id: body.message_id, scheduler_node: returnedRoute }
    },
  }
  Object.assign(connection, {
    async rpcWithProof(method: string, body: Record<string, unknown>, beforeSend: (proof: unknown) => void, _facts: unknown, replay?: { body: Record<string, unknown>; invocation: Record<string, unknown> }) {
      if (method === "create_bot" && failPrepare) throw new Error("prepare unavailable")
      const finalBody = method === "execution_control" ? { method: "operate", space_id: "space", resource_id: connection.project.id, authorization_id: body.operation_id, request: { ...localRequest, operation_id: body.operation_id, expected_revision: body.expected_revision, basis_heads: body.basis_heads, resources: [], action: body.action } } : body
      const proof = replay ?? { body: structuredClone(finalBody), invocation: { nonce: crypto.randomUUID() } }
      assert.deepEqual(JSON.parse(JSON.stringify(proof.body)), JSON.parse(JSON.stringify(finalBody)))
      beforeSend?.(proof)
      return { ...proof, result: await connection.rpc(method, proof.body) }
    },
  })
  const render = () => {
    cursor = 0
    const state = useWorkspaceState(connection)
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
    scheduler = { jobs: [{ trigger: 'queued-trigger', state: 'queued', kind: 'human', sequence: 1, target_node: 'scheduler', attempt_id: null }] }
    await state.refresh()
    await render().cancelManager('queued-trigger')
    assert.deepEqual(managerControls, [{ conversation_id: 'chat', trigger: 'queued-trigger', target_node: 'scheduler', attempt_id: null }])
    scheduler = { jobs: [{ trigger: 'missing-attempt', state: 'running', kind: 'human', sequence: 1, target_node: 'scheduler' }] }
    await render().refresh()
    await assert.rejects(render().cancelManager('missing-attempt'), /缺少完整/)
    assert.equal(managerControls.length, 1)
    scheduler = null
    await render().refresh()
    state = render()
    await state.sendMessage("work")
    state = render()
    assert.ok(state.messagePending)
    const pending = JSON.parse(JSON.stringify(state.messagePending))
    assert.deepEqual(messages[0], { conversation_id: "chat", message_id: pending.message_id, text: "work", basis_heads: ["head"], scheduler_node: null, entry_node: "project-node", report_results: true })
    assert.equal(pending.scheduler_node, null)
    assert.equal(pending.selected_route, "chosen")
    const firstProof = structuredClone(pending.attempt)
    returnedRoute = "other-scheduler"
    await state.sendMessage("work")
    assert.equal(render().messagePending?.selected_route, "chosen")
    returnedRoute = "chosen"
    assert.deepEqual(messages[1], messages[0])
    assert.deepEqual(JSON.parse(JSON.stringify(render().messagePending?.attempt)), firstProof)
    await assert.rejects(render().sendMessage("different work"), /原消息/)
    connection.audience.device = "other-project"
    await assert.rejects(render().sendMessage("work"), /原入口/)
    connection.audience.device = "project-node"
    await assert.rejects(state.executionControl("execution", { type: "respond_request", request_id: 7, response: { decision: "decline" } }), /connection lost/)
    state = render()
    assert.equal(state.pendingControls.length, 1)
    assert.deepEqual((controls[0].request as Record<string, unknown>).action, { type: "respond_request", request_id: 7, response: { decision: "decline" } })
    assert.equal(controls[0].authorization_id, state.pendingControls[0].operation_id)
    await assert.rejects(state.executionControl("execution", { type: "interrupt" }), /原操作待核对/)
    execution.revision = 9
    failControl = false
    await state.retryExecutionControl(state.pendingControls[0])
    assert.deepEqual(controls[1], controls[0])
    assert.equal(render().pendingControls.length, 0)
    await assert.rejects(render().stopFollowup("execution"), /缺少已核验的工单/)
    assert.equal(followups.length, 0)
    await assert.rejects(render().createBot({ name: "large", instructions: "中".repeat(2731) }), /8192/)
    assert.equal(render().pendingBot, null)
    await assert.rejects(render().createBot({ name: "n".repeat(512), instructions: "a".repeat(8192), project: { target_node: "executor", cwd: "/" + "p".repeat(4095) } }), /12000/)
    assert.equal(render().pendingBot, null)
    failPrepare = true
    await assert.rejects(render().createBot({ name: "not signed", instructions: "" }), /prepare unavailable/)
    assert.equal(render().pendingBot, null)
    failPrepare = false
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
  } finally {
    for (const effect of effects.values()) effect.cleanup?.()
  }
})

test("taskless execution projection and persisted approval actions retain nullable session identity", () => {
  const execution = { operation_id: "command", target_node: "executor", session_id: null, revision: 0, title: "查询状态", state: "succeeded", session_state: "none", capabilities: [], pending_requests: [], freshness: "local_ledger", detail: "退出码 0\nclean", blobs: [] }
  const payload = parseConversation({ ...context, executions: [execution], operations: [] })
  assert.deepEqual(JSON.parse(JSON.stringify(payload.executions[0])), execution)
  assert.equal(payload.executions[0].session_id, null)
  assert.throws(() => parseConversation({ ...context, executions: [{ ...execution, state: "idle" }] }))
  const execution_control = { basis_heads: [], conversation_id: "chat", operation_id: "approval", expected_operation_id: "execution", expected_revision: 4, action: { type: "respond_request", request_id: 1, response: { decision: "decline" } } }
  const pending = { operation_id: "approval", target_node: "executor", conversation_id: "chat", session_id: "session", execution_control, attempt: null, control: null }
  assert.deepEqual(parsePendingControls([pending])[0], pending)
  assert.throws(() => parsePendingControls([{ ...pending, execution_control: { ...execution_control, conversation_id: "other" } }]))
  assert.throws(() => parsePendingControls([{ ...pending, execution_control: { ...execution_control, action: { type: "respond_request", request_id: 1 } } }]))
})

test("future processing can be revoked while the current execution is unknown and offline", async () => {
  const elements: { type: unknown; props: Record<string, unknown> }[] = [], stopped: string[] = []
  const jsx = (type: unknown, props: Record<string, unknown>) => { const element = { type, props }; elements.push(element); return element }
  const ui = new Proxy({}, { get: (_target, name) => String(name) })
  const module: { exports: Record<string, unknown> } = { exports: {} }
  const require = (specifier: string) => {
    if (specifier === "react") return { useId: () => "work", useState: (initial: unknown) => [initial, () => {}] }
    if (specifier === "react/jsx-runtime") return { jsx, jsxs: jsx }
    if (specifier === "../identity/context") return { useDevice: () => ({ directory: { devices: [{ device_id: "executor" }] } }) }
    if (specifier === "./context") return { useWorkspace: () => ({ pendingControls: [], nodes: [], canPerform: () => true, writable: true, selectedBot: null, connectionOnline: true, run: (action: () => Promise<void>) => action(), stopFollowup: async (operationId: string) => { stopped.push(operationId) } }) }
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
  const stop = elements.find(({ props }) => props.children === "停止后续处理")
  assert.ok(stop && typeof stop.props.action === "function")
  assert.equal(stop.props.disabled, false)
  assert.equal(elements.find(({ props }) => props.children === "停止当前执行")?.props.disabled, true)
  await stop.props.action()
  assert.deepEqual(stopped, ["execution"])
  elements.length = 0
  ExecutionActivity({ execution: { ...execution, goal: { ...execution.goal, followup_mode: "off" } }, conversationId: "chat" })
  assert.equal(elements.some(({ props }) => props.children === "停止后续处理"), false)
})


test("device vault encrypts secrets and rejects wrong passwords, tampering and old formats", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, "")).toString("base64")}`)
  const secret = "09".repeat(32)
  const password = "a local passphrase 123"
  const first = await transport.sealDeviceVault(secret, password)
  const second = await transport.sealDeviceVault(secret, password)
  assert.equal(await transport.openDeviceVault(first, password), secret)
  assert.equal(first.secret, undefined)
  assert.equal(JSON.stringify(first).includes(secret), false)
  assert.notEqual(first.salt, second.salt)
  assert.notEqual(first.iv, second.iv)
  assert.notEqual(first.ciphertext, second.ciphertext)
  await assert.rejects(transport.openDeviceVault(first, "wrong password"))
  await assert.rejects(transport.openDeviceVault({ ...first, ciphertext: `${first.ciphertext[0] === "0" ? "1" : "0"}${first.ciphertext.slice(1)}` }, password))
  await assert.rejects(transport.openDeviceVault({ ...first, iterations: 1 }, password))
  await assert.rejects(transport.openDeviceVault({ ...first, secret }, password))
  await assert.rejects(transport.openDeviceVault({ secret, created_at: 1 }, password))
  await assert.rejects(transport.sealDeviceVault(secret, "short"))
})

test("local PRF wrap authenticates metadata and keeps the independent password vault usable", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, "") + "\nexport { sealPrfVault, openPrfVault, parseProfile };\n").toString("base64")}`)
  const savedLocation = Object.getOwnPropertyDescriptor(globalThis, "location")
  Object.defineProperty(globalThis, "location", { configurable: true, value: { origin: "https://client.example", hostname: "client.example" } })
  try {
    const secret = "09".repeat(32), password = "an independent local passphrase"
    const profile = { format: "vibeface-device-profile-v3", id: "11111111-1111-4111-8111-111111111111", name: "account", public_key: "02".repeat(32), vault: await transport.sealDeviceVault(secret, password) }
    const salt = crypto.getRandomValues(new Uint8Array(32))
    const prf = new Uint8Array(32).fill(9)
    const passkey = await transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt, prf })
    assert.deepEqual(prf, new Uint8Array(32))
    assert.equal(passkey.kdf, "HKDF-SHA256")
    assert.equal(passkey.salt.length, 64)
    assert.equal(passkey.iv.length, 24)
    assert.equal(passkey.ciphertext.length, 96)
    assert.equal(JSON.stringify(passkey).includes(secret), false)
    assert.deepEqual(transport.parseProfile(profile), profile)
    assert.deepEqual(transport.parseProfile({ ...profile, passkey }).vault, profile.vault)
    assert.equal(await transport.openDeviceVault(profile.vault, password), secret)
    const correct = new Uint8Array(32).fill(9)
    assert.equal(await transport.openPrfVault({ ...profile, passkey }, correct), secret)
    assert.deepEqual(correct, new Uint8Array(32))
    const second = await transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt, prf: new Uint8Array(32).fill(9) })
    assert.notEqual(second.iv, passkey.iv)
    assert.notEqual(second.ciphertext, passkey.ciphertext)
    const wrong = new Uint8Array(32).fill(8)
    await assert.rejects(transport.openPrfVault({ ...profile, passkey }, wrong))
    assert.deepEqual(wrong, new Uint8Array(32))
    const changeHex = (value: string) => `${value[0] === "0" ? "1" : "0"}${value.slice(1)}`
    for (const changed of [
      { credential_id: "BAUG" }, { origin: "https://other.example", rp_id: "other.example" }, { rp_id: "other.example" },
      { salt: changeHex(passkey.salt) }, { iv: changeHex(passkey.iv) }, { ciphertext: changeHex(passkey.ciphertext) },
    ]) {
      const output = new Uint8Array(32).fill(9)
      await assert.rejects(transport.openPrfVault({ ...profile, passkey: { ...passkey, ...changed } }, output))
      assert.deepEqual(output, new Uint8Array(32))
    }
    await assert.rejects(transport.openPrfVault({ ...profile, id: "22222222-2222-4222-8222-222222222222", passkey }, new Uint8Array(32).fill(9)))
    await assert.rejects(transport.openPrfVault({ ...profile, public_key: "03".repeat(32), passkey }, new Uint8Array(32).fill(9)))
    assert.throws(() => transport.parseProfile({ ...profile, passkey: { ...passkey, extra: true } }))
    assert.throws(() => transport.parseProfile({ ...profile, passkey: { ...passkey, credential_id: "AQID=" } }))
    assert.throws(() => transport.parseProfile({ ...profile, passkey: null }))
    const short = new Uint8Array(31).fill(9)
    await assert.rejects(transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt, prf: short }))
    assert.deepEqual(short, new Uint8Array(31))
    const invalidSaltPrf = new Uint8Array(32).fill(9)
    await assert.rejects(transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt: salt.slice(0, 31), prf: invalidSaltPrf }))
    assert.deepEqual(invalidSaltPrf, new Uint8Array(32))
  } finally {
    if (savedLocation) Object.defineProperty(globalThis, "location", savedLocation)
    else Reflect.deleteProperty(globalThis, "location")
  }
})

test("local auto-unlock cancellation aborts both binding and active-profile writes", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, "") + "\nexport { localVaultRecord };\n").toString("base64")}`)
  const saved = Object.getOwnPropertyDescriptor(globalThis, "indexedDB")
  const records = new Map<string, unknown>([["remote-profile", { untouched: true }]])
  let beforeCommit: (() => void) | null = null
  let onOpen: (() => void) | null = null
  let opened = 0
  const db = { close() {}, transaction() {
    let aborted = false, pending = 0
    const staged = new Map<string, unknown>()
    const transaction = { oncomplete: () => {}, onerror: () => {}, onabort: () => {}, abort() { aborted = true; queueMicrotask(() => transaction.onabort()) }, objectStore: () => ({
      get: (key: string) => request(records.get(key)),
      put: (value: unknown, key: string) => { staged.set(key, structuredClone(value)); return request(key) },
    }) }
    function request(result: unknown) {
      pending++
      queueMicrotask(() => {
        pending--
        if (!pending && !aborted) queueMicrotask(() => {
          beforeCommit?.()
          if (aborted) return
          for (const [key, value] of staged) records.set(key, value)
          transaction.oncomplete()
        })
      })
      return { result }
    }
    return transaction
  } }
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: { open() {
    opened++
    const opening = { result: db, onsuccess: () => {} }
    queueMicrotask(() => { onOpen?.(); opening.onsuccess() })
    return opening
  } } })
  try {
    const record = { nodeId: "node", profileId: "profile", vault: "wrapped" }
    const cancelled = new AbortController()
    cancelled.abort()
    await assert.rejects(transport.localVaultRecord("binding", record, undefined, "node", cancelled.signal), /取消/)
    assert.equal(opened, 0)
    const duringOpen = new AbortController()
    onOpen = () => duringOpen.abort()
    await assert.rejects(transport.localVaultRecord("binding", record, undefined, "node", duringOpen.signal), /取消/)
    onOpen = null
    const beforeCommitController = new AbortController()
    beforeCommit = () => beforeCommitController.abort()
    await assert.rejects(transport.localVaultRecord("binding", record, undefined, "node", beforeCommitController.signal), /未保存/)
    assert.deepEqual([...records], [["remote-profile", { untouched: true }]])
    beforeCommit = null
    await transport.localVaultRecord("binding", record, undefined, "node", new AbortController().signal)
    assert.deepEqual(records.get("binding"), record)
    assert.equal(records.get("active:node"), "profile")
    assert.deepEqual(records.get("remote-profile"), { untouched: true })
  } finally {
    if (saved) Object.defineProperty(globalThis, "indexedDB", saved)
    else Reflect.deleteProperty(globalThis, "indexedDB")
  }
})

test("local PRF profile writes preserve other profiles and reject changed or cancelled writers", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const shim = `const init = async () => {}; const Identity = { fromSecretHex(secret) {
    if (secret !== '09'.repeat(32)) throw new Error('wrong secret');
    return { secretHex: () => secret, publicKeyHex: () => '02'.repeat(32), nodeId: () => '03'.repeat(32), free() {} };
  } };`
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, shim + "\n")).toString("base64")}`)
  const saved = new Map(["isSecureContext", "location", "navigator", "indexedDB"].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  const records = new Map<string, unknown>(), locks: string[] = [], writes: string[] = []
  let beforeUpdate: (() => void) | null = null
  let lockGate: Promise<void> | null = null
  const db = { close() {}, transaction(_store: string, mode: string) {
    let aborted = false, pending = 0
    const transaction = { oncomplete: () => {}, onerror: () => {}, onabort: () => {}, abort() { aborted = true; queueMicrotask(() => transaction.onabort()) }, objectStore: () => ({
      get: (id: string) => request(() => structuredClone(records.get(id))), getAll: () => request(() => structuredClone([...records.values()])),
      put: (value: unknown, id: string) => request(() => { writes.push(id); records.set(id, structuredClone(value)) }),
    }) }
    function request(action: () => unknown) {
      const output: { result: unknown; onsuccess: () => void } = { result: undefined, onsuccess: () => {} }
      pending++
      queueMicrotask(() => {
        if (aborted) return
        try { output.result = action(); output.onsuccess() } catch { transaction.abort() }
        pending--
        if (!pending && !aborted) queueMicrotask(() => { if (!pending && !aborted) transaction.oncomplete() })
      })
      return output
    }
    if (mode === "readwrite") beforeUpdate?.()
    return transaction
  } }
  const properties = {
    isSecureContext: true, location: { origin: "https://client.example", hostname: "client.example" },
    navigator: { locks: { async request(name: string, work: () => Promise<void>) { locks.push(name); if (lockGate) await lockGate; await work() } } },
    indexedDB: { open() { const output = { result: db, onsuccess: () => {} }; queueMicrotask(() => output.onsuccess()); return output } },
  }
  for (const [key, value] of Object.entries(properties)) Object.defineProperty(globalThis, key, { configurable: true, value })
  try {
    const password = "a separate local passphrase"
    const first = { format: "vibeface-device-profile-v3", id: "11111111-1111-4111-8111-111111111111", name: "first", public_key: "02".repeat(32), vault: await transport.sealDeviceVault("09".repeat(32), password) }
    const other = { ...first, id: "22222222-2222-4222-8222-222222222222", name: "second" }
    records.set(first.id, first); records.set(other.id, other)
    const options = () => ({ credentialId: "AQID", salt: crypto.getRandomValues(new Uint8Array(32)), prf: new Uint8Array(32).fill(9) })
    await transport.initClient({ mode: "unlock", profileId: first.id, password })
    const valid = options()
    await transport.protectClientVaultWithPrf(valid)
    assert.deepEqual(valid.prf, new Uint8Array(32))
    assert.deepEqual(writes, [first.id])
    assert.equal(locks[0], `vibeface-device-vault-v3:${first.id}:writer`)
    assert.deepEqual(records.get(other.id), other)
    const backup = JSON.parse(await transport.exportDeviceVault())
    assert.deepEqual(backup.vault, first.vault)
    const local = await transport.passkeyVaultUnlockOptions(first.id)
    assert.equal(local.credentialId, "AQID")
    assert.deepEqual(local.options.publicKey.allowCredentials, [{ type: "public-key", id: "AQID" }])
    assert.equal(local.options.publicKey.rpId, "client.example")
    assert.equal(local.options.publicKey.userVerification, "required")
    assert.deepEqual(local.salt, Uint8Array.from(backup.passkey.salt.match(/../g), (byte: string) => Number.parseInt(byte, 16)))
    assert.notEqual(local.options.publicKey.challenge, (await transport.passkeyVaultUnlockOptions(first.id)).options.publicKey.challenge)
    transport.lockClient()
    const unlockPrf = new Uint8Array(32).fill(9)
    assert.equal((await transport.initClient({ mode: "unlock_prf", profileId: first.id, prf: unlockPrf })).profileId, first.id)
    assert.deepEqual(unlockPrf, new Uint8Array(32))
    beforeUpdate = () => records.set(first.id, { ...backup, name: "changed in another tab" })
    const changed = options()
    await assert.rejects(transport.protectClientVaultWithPrf(changed), /保存/)
    assert.deepEqual(changed.prf, new Uint8Array(32))
    assert.deepEqual(writes, [first.id])
    beforeUpdate = null
    let releaseLock = () => {}
    lockGate = new Promise<void>(resolve => { releaseLock = resolve })
    const cancelled = options(), pending = transport.protectClientVaultWithPrf(cancelled)
    transport.lockClient(); releaseLock()
    await assert.rejects(pending, /改变/)
    assert.deepEqual(cancelled.prf, new Uint8Array(32))
    assert.deepEqual(writes, [first.id])
    const unavailable = options()
    await assert.rejects(transport.protectClientVaultWithPrf(unavailable), /解锁/)
    assert.deepEqual(unavailable.prf, new Uint8Array(32))
    const invalid = new Uint8Array(32).fill(9)
    await assert.rejects(transport.initClient({ mode: "unlock_prf", profileId: "invalid", prf: invalid }))
    assert.deepEqual(invalid, new Uint8Array(32))
    assert.deepEqual(records.get(other.id), other)
  } finally {
    transport.lockClient()
    for (const [key, descriptor] of saved) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else Reflect.deleteProperty(globalThis, key)
    }
  }
})


test("local records and writer locks stay in their exact account space and device", () => {
  const scope = { account_id: "account:a", space_id: "space:a", device_id: "device:a" }
  const original = scopeKey(scope, "pending")
  assert.equal(scopeKey({ ...scope }, "pending"), original)
  assert.notEqual(scopeKey({ ...scope, account_id: "account:b" }, "pending"), original)
  assert.notEqual(scopeKey({ ...scope, space_id: "space:b" }, "pending"), original)
  assert.notEqual(scopeKey({ ...scope, device_id: "device:b" }, "pending"), original)
  assert.notEqual(scopeKey(scope, "frontiers"), original)
  assert.notEqual(scopeKey({ account_id: "a.b", space_id: "c", device_id: "d" }, "pending"), scopeKey({ account_id: "a", space_id: "b.c", device_id: "d" }, "pending"))
  assert.throws(() => scopeKey({ ...scope, account_id: "" }, "pending"))
  const first = `res2_${"a".repeat(64)}`, second = `res2_${"b".repeat(64)}`
  assert.notEqual(projectScopeKey(scope, first, "pending"), projectScopeKey(scope, second, "pending"))
  assert.notEqual(projectScopeKey(scope, first, "pending"), projectScopeKey(scope, first, "workspace-writer"))
  assert.throws(() => projectScopeKey(scope, "", "pending"))
  assert.throws(() => projectScopeKey(scope, scope.account_id, "pending"))
})


test("unknown writes remain in their original scope while another account is active", () => {
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window")
  const records = new Map<string, string>()
  Object.defineProperty(globalThis, "window", { configurable: true, value: { localStorage: { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => records.set(key, value) } } })
  try {
    const first = { account_id: "account:a", space_id: "space:a", device_id: "device:a" }
    const second = { account_id: "account:b", space_id: "space:b", device_id: "device:b" }
    const unknown = { ...request, state: "unknown" }
    writeStorage(scopeKey(first, "pending"), [unknown], true)
    assert.deepEqual(readStorage(scopeKey(second, "pending"), [], true), [])
    writeStorage(scopeKey(second, "pending"), [{ ...request, operation_id: "other" }], true)
    assert.deepEqual(readStorage(scopeKey(first, "pending"), [], true), [unknown])
    assert.equal(reconcileDecision(parseReconcile({ operation: { request, state: "unknown" } }), unknown), "pending")
  } finally {
    if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow)
    else Reflect.deleteProperty(globalThis, "window")
  }
})

test("only the new typed verified scope crosses the client boundary", () => {
  const scope = { account_id: `acc2_${"a".repeat(64)}`, space_id: `spc2_${"b".repeat(64)}`, device_id: `dev2_${"c".repeat(64)}` }
  assert.deepEqual(parseIdentityScope(scope), scope)
  assert.throws(() => parseIdentityScope({ ...scope, account_id: scope.device_id }))
  assert.throws(() => parseIdentityScope({ network_id: "a".repeat(64), subject_id: "b".repeat(64) }))
  assert.throws(() => parseIdentityScope({ ...scope, actor: "service" }))
  assert.throws(() => Reflect.apply(scopeKey, undefined, [undefined, "pending"]))
})


test("locking during initialization releases candidates and preserves temporary mode when storage fails", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const originalSecure = Object.getOwnPropertyDescriptor(globalThis, "isSecureContext")
  const originalDatabase = Object.getOwnPropertyDescriptor(globalThis, "indexedDB")
  Object.defineProperty(globalThis, "isSecureContext", { configurable: true, value: true })
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: { open() { const request: { onerror?: () => void } = {}; queueMicrotask(() => request.onerror?.()); return request } } })
  async function fixture(scenario: string) {
    const shim = `
      const scenario = ${JSON.stringify(scenario)};
      const initGate = Promise.withResolvers(), kdfGate = Promise.withResolvers(), entered = Promise.withResolvers();
      const stats = { generated: 0, freed: 0 };
      const init = () => initGate.promise;
      const Identity = { generate() {
        stats.generated++; let released = false;
        return { secretHex: () => '01'.repeat(32), publicKeyHex: () => '02'.repeat(32), nodeId: () => '03'.repeat(32),
          free() { if (released) throw new Error('double free'); released = true; stats.freed++; } };
      } };
      const crypto = { getRandomValues: bytes => globalThis.crypto.getRandomValues(bytes), randomUUID: () => globalThis.crypto.randomUUID(),
        subtle: { importKey: async () => ({}), deriveKey: async () => { entered.resolve(); await kdfGate.promise; return {}; }, encrypt: async () => new Uint8Array(48).buffer } };
      export const probe = { initGate, kdfGate, entered, stats };
    `
    return import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, shim)).toString("base64")}`)
  }
  try {
    const duringCrypto = await fixture("crypto")
    duringCrypto.probe.initGate.resolve()
    const creating = duringCrypto.initClient({ mode: "create", name: "test", password: "test passphrase 123" })
    await duringCrypto.probe.entered.promise
    duringCrypto.lockClient()
    assert.equal(duringCrypto.probe.stats.freed, 1)
    duringCrypto.probe.kdfGate.resolve()
    await assert.rejects(creating, /取消/)
    assert.equal(duringCrypto.probe.stats.freed, 1)
    assert.equal((await duringCrypto.clientVaultStatus()).status, "unavailable")
    assert.equal((await duringCrypto.initClient({ mode: "temporary" })).profileId, null)
    await assert.rejects(duringCrypto.initClient({ mode: "create", name: "other", password: "test passphrase 123" }), /先锁定/)
    duringCrypto.lockClient()
    assert.equal(duringCrypto.probe.stats.freed, 2)
    const duringInit = await fixture("init")
    const canceled = duringInit.initClient({ mode: "create", name: "test", password: "test passphrase 123" })
    duringInit.lockClient()
    duringInit.probe.kdfGate.resolve()
    duringInit.probe.initGate.resolve()
    await assert.rejects(canceled, /取消/)
    assert.equal(duringInit.probe.stats.generated, 0)
  } finally {
    if (originalSecure) Object.defineProperty(globalThis, "isSecureContext", originalSecure)
    else Reflect.deleteProperty(globalThis, "isSecureContext")
    if (originalDatabase) Object.defineProperty(globalThis, "indexedDB", originalDatabase)
    else Reflect.deleteProperty(globalThis, "indexedDB")
  }
})


test("saved policy heads cannot move backwards or replace an account root", async () => {
  const source = await readFile(new URL("../../lib/authorization.ts", import.meta.url), "utf8")
  const trustBoundary = await import("../../lib/authorization-trust.ts")
  const boundary = await import(`data:text/javascript;base64,${Buffer.from(stripTypeScriptTypes(source.replace(/^(?:import .*|export \{ .* from .*)\n/gm, ""))).toString("base64")}`)
  assert.equal(boundary.subscriptionRenewalDelay(120000, 0), 60000)
  assert.equal(boundary.subscriptionRenewalDelay(1000, 800), 100)
  assert.throws(() => boundary.subscriptionRenewalDelay(1000, 1000))
  const domain: Parameters<typeof trustBoundary.mergeAuthorizationTrust>[0]["pins"][number]["domain"] = { kind: "account", id: `acc2_${"a".repeat(64)}` }
  const firstHead = { domain, authority_epoch: 1, sequence: 1, head: `h2_${"1".repeat(64)}` }
  const latestHead = { ...firstHead, sequence: 2, head: `h2_${"2".repeat(64)}` }
  const trust: Parameters<typeof trustBoundary.mergeAuthorizationTrust>[0] = { kind: "policy", accountId: domain.id, spaceId: `spc2_${"b".repeat(64)}`, pins: [{ domain, genesis_root: "a".repeat(64), minimum_head: firstHead }] }
  const latest = { ...trust, pins: [{ ...trust.pins[0], minimum_head: latestHead }] }
  assert.deepEqual(trustBoundary.mergeAuthorizationTrust(trust, [latest]).pins[0].minimum_head, latestHead)
  assert.deepEqual(trustBoundary.mergeAuthorizationTrust(latest, [trust]).pins[0].minimum_head, latestHead)
  assert.throws(() => trustBoundary.mergeAuthorizationTrust({ ...trust, accountId: `acc2_${"f".repeat(64)}` }, [latest]))
  assert.throws(() => trustBoundary.mergeAuthorizationTrust({ ...trust, pins: [{ ...trust.pins[0], genesis_root: "f".repeat(64) }] }, [latest]))
  assert.throws(() => trustBoundary.mergeAuthorizationTrust({ ...trust, pins: [{ ...trust.pins[0], minimum_head: { ...latestHead, head: `h2_${"3".repeat(64)}` } }] }, [latest]))
  const switchedAuthority = { ...firstHead, authority_epoch: 2, sequence: 0, head: `h2_${"4".repeat(64)}` }
  assert.deepEqual(trustBoundary.mergeAuthorizationTrust(trust, [{ ...latest, pins: [{ ...latest.pins[0], minimum_head: switchedAuthority }] }]).pins[0].minimum_head, switchedAuthority)
})


test("RPC action and authorization identity come from the exact local request", () => {
  const first = prepareRpc({ method: "operate", params: { request: { operation_id: "original", action: { type: "send" } } }, spaceId: "space", authorizationId: "random" })
  assert.equal(first.authorizationId, "original")
  assert.equal(first.action, "execution_send")
  assert.equal(first.body.space_id, "space")
  assert.equal(first.body.authorization_id, "original")
  assert.equal(rpcAction("bots", {}), "conversation_read")
  assert.equal(rpcAction("create_bot", {}), "conversation_create")
  assert.equal(rpcAction("read_artifact", {}), "attachment_read")
  assert.equal(rpcAction("execution_status", {}), "operation_status_read")
  assert.equal(rpcAction("cancel_manager", {}), "agent_interrupt")
  assert.equal(rpcAction("set_followup", {}), "task_followup")
  assert.equal(rpcAction("resolve_manager_unknown", {}), "operation_resolve")
  for (const [type, action] of [["steer", "execution_steer"], ["interrupt", "execution_interrupt"], ["respond_request", "execution_respond_approval"]]) {
    assert.equal(rpcAction("execution_control", { action: { type } }), action)
    assert.equal(rpcAction("task_control", { action: type }), action)
  }
  assert.equal(rpcAction("task_control", { action: "status" }), "operation_status_read")
  assert.equal(prepareRpc({ method: "execution_control", params: { operation_id: "control", action: { type: "interrupt" } }, spaceId: "space", authorizationId: "random" }).authorizationId, "control")
  assert.equal(prepareRpc({ method: "task_control", params: { action: "status" }, spaceId: "space", authorizationId: "read" }).authorizationId, "read")
  assert.throws(() => rpcAction("task_control", { action: "accept" }))
  assert.equal(rpcAction("update_task", { status: "accepted" }), "task_accept")
  assert.equal(rpcAction("update_task", { status: "cancelled" }), "task_cancel")
  assert.equal(rpcAction("update_task", { status: "running" }), "task_progress")
  const read = prepareRpc({ method: "reconcile", params: { operation_id: "original" }, spaceId: "space", authorizationId: "new-query" })
  assert.deepEqual(read.body, { operation_id: "original", method: "reconcile", space_id: "space", authorization_id: "new-query" })
  assert.equal(read.authorizationId, "new-query")
  assert.deepEqual(prepareRpc({ method: "message", params: { message_id: "same" }, spaceId: "space", authorizationId: "a" }).body, prepareRpc({ method: "message", params: { message_id: "same" }, spaceId: "space", authorizationId: "b" }).body)
  assert.throws(() => prepareRpc({ method: "conversation", params: { space_id: "other" }, spaceId: "space", authorizationId: "read" }))
  assert.throws(() => prepareRpc({ method: "conversation", params: { method: "export" }, spaceId: "space", authorizationId: "read" }))
  assert.throws(() => rpcAction("unrecognized", {}))
  assert.throws(() => rpcAction("operate", { request: { action: { type: "unrecognized" } } }))
  assert.throws(() => prepareRpc({ method: "message", params: {}, spaceId: "space", authorizationId: "read" }))
})

test("Passkey requests pin the client RP and keep PRF output out of the Node response", async () => {
  const moduleUrl = (source: string) => `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
  const transport = moduleUrl((await readFile(new URL("../../../transport.js", import.meta.url), "utf8")).replace(/^import init,.*\n/, ""))
  const nodeSource = (await readFile(new URL("../../lib/node.ts", import.meta.url), "utf8")).replace("'../../transport.js'", JSON.stringify(transport))
  const node = moduleUrl(stripTypeScriptTypes(nodeSource))
  const source = (await readFile(new URL("../../lib/passkey.ts", import.meta.url), "utf8")).replace("'./node'", JSON.stringify(node))
  const passkey = await import(moduleUrl(stripTypeScriptTypes(source)))
  const saved = new Map(["isSecureContext", "PublicKeyCredential", "AuthenticatorAttestationResponse", "location", "navigator"].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  let calls = 0
  class Attestation {
    clientDataJSON = Uint8Array.from([4, 5]).buffer
    attestationObject = Uint8Array.from([6, 7]).buffer
    getTransports() { return ["internal"] }
  }
  class Credential {
    id = "AQID"
    rawId = Uint8Array.from([1, 2, 3]).buffer
    response = new Attestation()
    getClientExtensionResults() { return { prf: { results: { first: new Uint8Array(32).fill(9).buffer } } } }
  }
  const controller = new AbortController()
  const properties = {
    isSecureContext: true, PublicKeyCredential: Credential, AuthenticatorAttestationResponse: Attestation,
    location: { hostname: "client.example", origin: "https://client.example", protocol: "https:" },
    navigator: { credentials: { create: async (options: CredentialCreationOptions) => {
      calls++
      assert.equal(options.signal, controller.signal)
      assert.equal(options.publicKey?.rp.id, "client.example")
      const challenge = options.publicKey?.challenge
      assert.ok(challenge instanceof ArrayBuffer)
      assert.deepEqual(new Uint8Array(challenge), new Uint8Array(32).fill(3))
      const salt = options.publicKey?.extensions?.prf?.eval?.first
      assert.ok(salt instanceof ArrayBuffer)
      assert.deepEqual(new Uint8Array(salt), new Uint8Array(32).fill(4))
      return new Credential()
    } } },
  }
  for (const [key, value] of Object.entries(properties)) Object.defineProperty(globalThis, key, { configurable: true, value })
  try {
    const options = { publicKey: {
      rp: { id: "client.example", name: "Vibeface" }, user: { id: "AQID", name: "account", displayName: "Vibeface account" },
      challenge: passkey.encodeBase64Url(new Uint8Array(32).fill(3).buffer), pubKeyCredParams: [{ type: "public-key", alg: -7 }],
      attestation: "none", authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "required" }, excludeCredentials: [],
    } }
    const result = await passkey.createPasskey(options, controller.signal, new Uint8Array(32).fill(4))
    assert.deepEqual(result.response, { id: "AQID", rawId: "AQID", type: "public-key", response: { clientDataJSON: "BAU", attestationObject: "Bgc", transports: ["internal"] }, clientExtensionResults: {} })
    assert.deepEqual(result.prf, new Uint8Array(32).fill(9))
    assert.equal(JSON.stringify(result.response).includes("prf"), false)
    await assert.rejects(passkey.createPasskey({ publicKey: { ...options.publicKey, rp: { id: "node.example", name: "other" } } }, controller.signal), /站点/)
    assert.equal(calls, 1)
    assert.throws(() => passkey.decodeBase64Url("AQID="))
    assert.throws(() => passkey.decodeBase64Url("AB"))
    assert.deepEqual(new Uint8Array(passkey.decodeBase64Url("AQID")), Uint8Array.from([1, 2, 3]))
  } finally {
    for (const [key, descriptor] of saved) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else Reflect.deleteProperty(globalThis, key)
    }
  }
})

test("only a durable scheduler job settles the unchanged pending message", () => {
  const pending = { message_id: "message", conversation_id: "chat", text: "work", scheduler_node: null, selected_route: "chosen", entry_node: "project-node", basis_heads: [], attempt: null }
  assert.equal(hasQueuedMessage(null, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ trigger: "message:message", state: "pending_delivery", kind: "human", sequence: 0, message_id: "message", target_node: "chosen" }] }, pending), false)
  assert.equal(hasQueuedMessage({ jobs: [{ trigger: "message:message", state: "queued", kind: "human", sequence: 1, message_id: "message", target_node: "chosen" }] }, pending), true)
})

test("live controls sign only the exact expanded operation and verified native turn", () => {
  const original = { ...request }
  const control = { originalRequest: original, basisHeads: ["head"], nativeSessionId: "native", turnId: "turn", pendingRequests: [{ id: 7, threadId: "native", turnId: "turn" }] }
  const facts = { control, execution: { sessionId: "session", executorKind: "codex", canonicalCwd: "/project", bindingGeneration: 1 } }
  const raw = { method: "execution_control", space_id: "space", resource_id: "resource", authorization_id: "control", operation_id: "control", conversation_id: "chat", expected_operation_id: "op", expected_revision: 4, basis_heads: ["head"], action: { type: "steer", text: "exact text" } }
  const expanded = { method: "operate", space_id: "space", resource_id: "resource", authorization_id: "control", request: { ...original, operation_id: "control", expected_revision: 4, basis_heads: ["head"], action: { type: "steer", text: "exact text", turn_id: "turn" }, resources: [] } }
  const evidence = { intent: raw, original_request: original, status: { session: { native_session_id: "native", session_id: "session", target_node: "executor", revision: 4 }, turn: { turn: { id: "turn" } }, snapshot: { threads: { native: { last_turn: { id: "turn", status: "inProgress" } } } } } }
  assert.doesNotThrow(() => verifyPreparedBody(raw, expanded, facts, evidence))
  assert.doesNotThrow(() => verifyPreparedBody(raw, expanded, facts))
  for (const changed of [{ ...expanded, resource_id: "other" }, { ...expanded, authorization_id: "other" }, { ...expanded, request: { ...expanded.request, target_node: "other" } }, { ...expanded, request: { ...expanded.request, action: { ...expanded.request.action, text: "rewritten" } } }, { ...expanded, request: { ...expanded.request, action: { ...expanded.request.action, turn_id: "other" } } }]) {
    assert.throws(() => verifyPreparedBody(raw, changed, facts, evidence))
  }
  assert.throws(() => verifyPreparedBody(raw, expanded, facts, { ...evidence, intent: { ...raw, expected_operation_id: "other" } }))
  assert.throws(() => verifyPreparedBody(raw, expanded, { ...facts, control: { ...control, turnId: "other" } }, evidence))
  const approval = { ...raw, action: { type: "respond_request", request_id: 7, response: { decision: "decline" } } }
  const expandedApproval = { ...expanded, request: { ...expanded.request, action: approval.action } }
  assert.doesNotThrow(() => verifyPreparedBody(approval, expandedApproval, facts, { ...evidence, intent: approval }))
  assert.throws(() => verifyPreparedBody(approval, expandedApproval, { ...facts, control: { ...control, pendingRequests: [] } }))
})

test("verified control metadata preserves RPC id types and rejects other turns", () => {
  const value = { turn_id: 'turn', pending_requests: [{ request_id: 7, turn_id: 'turn' }, { request_id: '7', turn_id: 'turn' }], expires_at_ms: 1234 }
  assert.deepEqual(parseRemoteExecutionControl(value), value)
  assert.throws(() => parseRemoteExecutionControl({ ...value, pending_requests: [{ request_id: {}, turn_id: 'turn' }] }))
  assert.throws(() => parseRemoteExecutionControl({ ...value, pending_requests: [{ request_id: 7, turn_id: 'other' }] }))
  assert.throws(() => parseRemoteExecutionControl({ ...value, expires_at_ms: Number.MAX_SAFE_INTEGER + 1 }))
})
