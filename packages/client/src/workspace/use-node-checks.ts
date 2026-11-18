import { useCallback, useEffect, useState } from "react"
import type { NodeReportSnapshot } from "./types.ts"

const checkInterval = 30_000
const maxReportAge = 60_000

type Check = {
  read: () => Promise<void>
  startedAt: number
  state: "checking" | "failed" | "done"
}

export function useNodeChecks({ report, refreshNodes, active, foreground }: {
  report: NodeReportSnapshot | null
  refreshNodes: (() => Promise<void>) | undefined
  active: boolean
  foreground: () => boolean
}) {
  const [check, setCheck] = useState<Check | null>(null)
  const [expiredAt, setExpiredAt] = useState<number | null>(null)
  const refresh = useCallback(() => {
    if (!active || !refreshNodes || !foreground()) return
    const next: Check = { read: refreshNodes, startedAt: Date.now(), state: "checking" }
    setCheck(next)
    const finish = (state: "failed" | "done") => setCheck(prior => prior === next ? { ...next, state } : prior)
    void refreshNodes().then(() => finish("done"), () => finish("failed"))
  }, [active, refreshNodes, foreground])

  useEffect(() => {
    if (!active || !refreshNodes) return
    refresh()
    const timer = setInterval(refresh, checkInterval)
    return () => clearInterval(timer)
  }, [active, refreshNodes, refresh])
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
    failed: current?.state === "failed" && (report?.receivedAt ?? 0) <= current.startedAt,
    refresh,
  }
}
