import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { cardState, nodeName, taskGroup } from "@vibeface/client/workspace/labels.ts"
import { inputText, parseArtifactPage, parseConversation, pendingChecks, parseNodes, attachmentName, signedOrder } from "@vibeface/client/workspace/model.ts"

import { context } from './entry-url-test-kit.test-support.ts'


test("bulk inbox actions re-arm focus after an intervening list update", () => {
  const source = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "pending-section.tsx"), "utf8")
  const start = source.indexOf("const action = async (kind")
  const action = source.slice(start, source.indexOf("\n  return <section", start))
  assert.ok(action.indexOf("held.current = { key: bulkFocusKey") >= 0)
  assert.ok(action.indexOf("held.current = { key: bulkFocusKey") < action.indexOf("setBusy(kind)"), "the action re-arms the sentinel before the list can change")
})


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
  const work = {
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    work_id: "work", trigger: "message:one", target_node: "worker", cwd: "/project", title: "检查项目", prompt: "检查项目", state: "running", summary: "进行中", outcome: null, result_text: "", failure_reason: null, resolved: { provider: "codex", model: null } }
  const scheduler = { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: "message:one", state: "queued", message_id: "one" }], unanswered: [], capabilities: { streaming: true, cancel: false, steer: false } }
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
  assert.equal(parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: "message:two", owner: "node", conversation_id: "c", state: "pending_delivery", message_id: "two" }], unanswered: [] } }).scheduler?.jobs[0].state, "pending_delivery")
  assert.throws(() => parseConversation({ ...context, scheduler: { scope_jobs: [], jobs: [] } }), "the unanswered list is always sent")
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
