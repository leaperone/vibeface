"use client"

import { useId, useState } from "react"
import { CircleQuestionMark } from "lucide-react"
import { ActionCard, Details } from "@/components/app/kit"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { ActionButton } from "./present"
import type { PendingCheck } from "./types"

/** Plain-language reason; the original text stays in the details. */
export function checkReason(check: PendingCheck): string {
  if (/restart/i.test(check.detail)) return "设备在助手工作时重启了，系统不会自动重做。"
  if (/stream closed|frame limit|disconnect|connection/i.test(check.detail)) return "和助手的连接断了，系统不会自动重做。"
  return "这次的结果没法自动确认，系统不会自动重做。"
}

function recheckSummary(result: Record<string, unknown>): string {
  if (result.pending === false) return "已经有最终结果了。"
  const groups = Array.isArray(result.process_groups) ? result.process_groups as { exited: boolean }[] : []
  if (!groups.length) return "还是查不到结果，请按实际情况选。"
  return groups.some((group) => !group.exited) ? "还有程序在运行，稍后再查。" : "相关程序都已结束，请按实际情况选。"
}

/** "需要确认": the person says whether interrupted work actually finished. Nothing is re-run. */
export function ConfirmCard({ check }: { check: PendingCheck }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const [note, setNote] = useState("")
  const [summary, setSummary] = useState("")
  const disabled = !workspace.writable || !workspace.selectedBot
  const native = check.kind === "work"
  const answer = (choice: "completed" | "interrupted") => async () => { await workspace.resolveCheck(check, choice, note) }
  return <ActionCard
    icon={CircleQuestionMark}
    title={`需要确认 · ${native ? "任务被打断" : "上次回复被打断"}`}
    meta={nodeName(check.target_node ?? workspace.data?.local_node, { nodes: workspace.nodes, directory })}
    actions={<>
      <ActionButton variant="ghost" disabled={disabled} action={async () => { setSummary(""); setSummary(recheckSummary(await workspace.reconcileCheck(check))) }}>不确定，再查一次</ActionButton>
      <ActionButton disabled={disabled} action={answer("interrupted")}>没做完</ActionButton>
      <ActionButton variant="default" disabled={disabled} action={answer("completed")}>做完了</ActionButton>
    </>}
  >
    <p>{checkReason(check)}{native ? "那个任务实际做完了吗？" : "那次要做的事实际做完了吗？"}</p>
    {summary ? <p role="status" className="text-meta text-muted-foreground">{summary}</p> : null}
    <Details label="补充说明（可选）"><Textarea id={`${id}-note`} aria-label="补充说明" rows={2} maxLength={2000} value={note} disabled={disabled} onChange={(event) => setNote(event.target.value)} /></Details>
    {check.detail ? <Details><span>{check.detail}</span></Details> : null}
  </ActionCard>
}
