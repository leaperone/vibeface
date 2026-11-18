import { useCallback, useEffect, useState } from "react"
import type { NodeReportSnapshot } from "./types.ts"

const checkInterval = 30_000
const maxReportAge = 60_000
type ReadNodes = (checkConnections?: boolean) => Promise<void>

type Check = { read: ReadNodes } & ({ state: "checking" | "done" } | { state: "failed"; failedAt: number })

export function useNodeChecks({ report, refreshNodes, active, foreground }: {
  report: NodeReportSnapshot | null
  refreshNodes: ReadNodes | undefined
  active: boolean
  foreground: () => boolean
}) {
  const [check, setCheck] = useState<Check | null>(null)
  const [expiredAt, setExpiredAt] = useState<number | null>(null)
  const checkNodes = useCallback((checkConnections: boolean) => {
    if (!active || !refreshNodes || !foreground()) return
    const next: Check = { read: refreshNodes, state: "checking" }
    setCheck(next)
    const finish = (completed: Check) => setCheck(prior => prior === next ? completed : prior)
    void refreshNodes(checkConnections).then(
      () => finish({ read: refreshNodes, state: "done" }),
      () => finish({ read: refreshNodes, state: "failed", failedAt: Date.now() }),
    )
  }, [active, refreshNodes, foreground])
  const refresh = useCallback(() => checkNodes(true), [checkNodes])

  useEffect(() => {
    if (!active || !refreshNodes) return
    checkNodes(false)
    const timer = setInterval(() => checkNodes(false), checkInterval)
    return () => clearInterval(timer)
  }, [active, refreshNodes, checkNodes])
  useEffect(() => {
    if (!active || !report) return
    const remaining = report.receivedAt + maxReportAge - Date.now()
    if (remaining <= 0) return
    const timer = setTimeout(() => setExpiredAt(report.receivedAt), remaining)
    return () => clearTimeout(timer)
  }, [active, report])

  const current = active && check?.read === refreshNodes ? check : null
  return {
    fresh: report !== null && expiredAt !== report.receivedAt && Date.now() - report.receivedAt < maxReportAge,
    checking: current?.state === "checking",
    failed: current?.state === "failed" && (report?.receivedAt ?? 0) <= current.failedAt,
    refresh,
  }
}
