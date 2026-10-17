import assert from "node:assert/strict"
import test from "node:test"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis } from "./labels.ts"
import { inputText, parseConversation, parseNodes, parsePending } from "./model.ts"
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

test("creation fields use the server UTF-8 byte limits and reject whitespace", () => {
  assert.equal(inputText("  名称  ", "会话名称", 512), "名称")
  assert.equal(inputText("中".repeat(170), "会话名称", 512), "中".repeat(170))
  assert.throws(() => inputText("中".repeat(171), "会话名称", 512), /512/)
  assert.throws(() => inputText("   ", "工单目标", 65536), /不能为空/)
  assert.equal(inputText("a".repeat(65536), "验收条件", 65536).length, 65536)
  assert.throws(() => inputText("中".repeat(21846), "验收条件", 65536), /65536/)
})
