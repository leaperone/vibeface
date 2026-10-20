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

/** "需要确认": the person says whether interrupted work finished, or that a leftover process stopped. Nothing is re-run. */
export function ConfirmCard({ check }: { check: PendingCheck }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const [note, setNote] = useState("")
  const disabled = !workspace.writable || !workspace.selectedBot
  const lookup = { nodes: workspace.nodes, directory }
  if (check.kind === "process") return <ActionCard
    icon={CircleQuestionMark}
    title="需要确认 · 上次回复的程序还在运行"
    meta={nodeName(workspace.data?.local_node, lookup)}
    actions={<ActionButton variant="default" disabled={disabled} action={() => workspace.confirmStopped(check)}>确认已停止</ActionButton>}
  >
    <p>{checkReason(check)}相关程序还没退出，退出后会自动处理。如果你确认它已经停了，点“确认已停止”。</p>
    {check.detail ? <Details><span>{check.detail}</span></Details> : null}
  </ActionCard>
  const answer = (choice: "completed" | "interrupted") => async () => { await workspace.resolveCheck(check, choice, note) }
  return <ActionCard
    icon={CircleQuestionMark}
    title="需要确认 · 任务被打断"
    meta={nodeName(check.target_node, lookup)}
    actions={<>
      <ActionButton disabled={disabled} action={answer("interrupted")}>没做完</ActionButton>
      <ActionButton variant="default" disabled={disabled} action={answer("completed")}>做完了</ActionButton>
    </>}
  >
    <p>{checkReason(check)}那个任务实际做完了吗？</p>
    <Details label="补充说明（可选）"><Textarea id={`${id}-note`} aria-label="补充说明" rows={2} maxLength={2000} value={note} disabled={disabled} onChange={(event) => setNote(event.target.value)} /></Details>
    {check.detail ? <Details><span>{check.detail}</span></Details> : null}
  </ActionCard>
}
