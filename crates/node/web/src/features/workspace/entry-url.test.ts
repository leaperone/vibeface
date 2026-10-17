import assert from "node:assert/strict"
import test from "node:test"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis } from "./labels.ts"
import { inputText, parseArtifactPage, parseConversation, parseNodes, parsePending } from "./model.ts"
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
