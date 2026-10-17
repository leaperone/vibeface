import { parseOperation, record } from "./model.ts"
import type { OperationRecord } from "./types.ts"

export type ReconcileRecord =
  | { operation: OperationRecord; actual?: unknown }
  | { operation: null; state: "not_found"; target_node: string; checked_at_ms: number }

export function parseReconcile(value: unknown): ReconcileRecord {
  const data = record(value)
  if (data.operation !== null) return { operation: parseOperation(data.operation), actual: data.actual }
  if (data.state !== "not_found" || typeof data.target_node !== "string" || typeof data.checked_at_ms !== "number" || !Number.isSafeInteger(data.checked_at_ms) || data.checked_at_ms <= 0) throw new Error("执行设备没有返回可核对的缺失记录。")
  return { operation: null, state: "not_found", target_node: data.target_node, checked_at_ms: data.checked_at_ms }
}
export function reconcileDecision(record: ReconcileRecord, pending: { operation_id: string; target_node: string; conversation_id: string; session_id: string }): "settled" | "missing" | "pending" {
  if (record.operation === null) return record.target_node === pending.target_node ? "missing" : "pending"
  if (record.operation.request.operation_id !== pending.operation_id || record.operation.request.target_node !== pending.target_node || record.operation.request.conversation_id !== pending.conversation_id || record.operation.request.session_id !== pending.session_id) throw new Error("执行设备返回了其他操作，已保留原待核对记录。")
  return ["succeeded", "failed", "rejected"].includes(record.operation.state) ? "settled" : "pending"
}
export function errorText(error: unknown) { return error instanceof Error ? error.message : String(error) }
