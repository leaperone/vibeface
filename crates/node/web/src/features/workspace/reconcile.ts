export type ReconcileRecord = {
  state?: string
  target_node?: string
  operation?: { state?: string } | null
}

export function reconcileDecision(
  record: ReconcileRecord,
  operation: { target_node: string },
): "settled" | "missing" | "pending" {
  const stateValue = record.state || record.operation?.state
  if (stateValue === "succeeded" || stateValue === "failed" || stateValue === "rejected") return "settled"
  if (stateValue === "not_found" && record.target_node === operation.target_node && record.operation === null) {
    return "missing"
  }
  return "pending"
}

export function shouldDropPending(status: number | undefined) {
  return status !== undefined && status >= 400 && status < 500
}

export function statusOf(error: unknown) {
  if (typeof error === "object" && error && "status" in error && typeof error.status === "number") return error.status
  return undefined
}

export function errorText(error: unknown) {
  return error instanceof Error ? error.message : String(error)
}
