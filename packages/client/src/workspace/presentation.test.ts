import assert from "node:assert/strict"
import test from "node:test"
import { inputDeliveryDescriptor, inputReceiptDescriptor, workspaceTaskCounts, workspaceTaskGroup, workspaceTaskStateDescriptor } from "./presentation.ts"
import type { PendingMessage } from "../chat/types.ts"
import type { InputReceipt, WorkspaceTask } from "./types.ts"

test("workspace task descriptors cover every state and preserve row/count grouping", () => {
  const states = Object.keys(workspaceTaskStateDescriptor) as WorkspaceTask["state"][]
  assert.equal(states.length, 10)
  assert.deepEqual(states.map((state) => workspaceTaskGroup(state)), ["waiting", "working", "waiting", "attention", "attention", "attention", "attention", null, null, "waiting"])
  assert.deepEqual(workspaceTaskCounts(states.map((state) => ({ state }))), { attention: 4, working: 1, waiting: 3 })
  assert.equal(workspaceTaskStateDescriptor.needs_reconciliation.attention, true)
  assert.equal(workspaceTaskStateDescriptor.running.active, true)
  assert.deepEqual(workspaceTaskStateDescriptor.completed.allowedActions, [])
})

test("input delivery and receipt descriptors are exhaustive, including unknown outcomes", () => {
  const deliveries = ["saved", "uploading", "submitting", "accepted", "unknown"] as PendingMessage["delivery"][]
  const receipts = ["entry_accepted", "owner_accepted", "submitting", "provider_accepted", "associated", "refused", "unknown"] as InputReceipt["stage"][]
  assert.deepEqual(deliveries.map((stage) => inputDeliveryDescriptor[stage].label), ["本地已保存", "本地已保存，附件准备中", "正在发送给连接的设备", "连接的设备已接收", "是否送达尚未确认"])
  assert.equal(inputDeliveryDescriptor.unknown.attention, true)
  assert.equal(inputReceiptDescriptor.unknown.attention, true)
  assert.equal(inputReceiptDescriptor.submitting.active, true)
  assert.equal(receipts.length, Object.keys(inputReceiptDescriptor).length)
})

test("an unknown state fails closed instead of being silently grouped", () => {
  assert.throws(() => Reflect.apply(workspaceTaskGroup, undefined, ["future_state"]), TypeError)
})
