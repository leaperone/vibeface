"use client"

import { useId, useState } from "react"
import { CircleQuestionMark, Unplug, WifiOff } from "lucide-react"
import { ActionCard, Details, toast } from "@/components/app/kit"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { ActionButton } from "./present"
import type { WorkCheck } from "./model"
import type { PendingCheck } from "./types"

/** Plain-language reason; the original text stays in the details. */
export function checkReason(check: Pick<PendingCheck, "detail">): string {
  if (/restart/i.test(check.detail)) return "设备在助手工作时重启了，系统不会自动重做。"
  if (/stream closed|frame limit|disconnect|connection/i.test(check.detail)) return "和助手的连接断了，系统不会自动重做。"
  return "这次的结果没法自动确认，系统不会自动重做。"
}

export function confirmText(check: Pick<WorkCheck, "reason" | "detail">, device: string): string {
  return check.reason === "offline" ? `${device} 离线了，报不回这个任务的结果。`
    : check.reason === "disconnected" ? `${device} 上的 Codex 断开了，报不回这个任务的结果。`
    : checkReason(check)
}

const confirmIcons = { offline: WifiOff, disconnected: Unplug, unverified: CircleQuestionMark }

/** "待确认": the person says whether a work its device cannot report finished, or that a leftover process stopped. Nothing is re-run. */
export function ConfirmCard({ check }: { check: PendingCheck }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const [note, setNote] = useState("")
  const disabled = !workspace.writable || check.kind === "process" && !workspace.selectedBot
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
  const answer = (choice: "completed" | "interrupted") => async () => { await workspace.resolveCheck(check, choice, note); toast(choice === "completed" ? "已记为做完" : "已记为没做完", { tone: "success" }) }
  const device = nodeName(check.target_node, lookup)
  const reason = confirmText(check, device)
  return <ActionCard
    icon={confirmIcons[check.reason]}
    title={`待确认 · ${check.title || "任务结果"}`}
    meta={device}
    actions={<>
      <ActionButton disabled={disabled} action={answer("interrupted")}>没做完</ActionButton>
      <ActionButton variant="default" disabled={disabled} action={answer("completed")}>做完了</ActionButton>
    </>}
  >
    <p>{reason}那个任务实际做完了吗？</p>
    <Details label="补充说明（可选）"><Textarea id={`${id}-note`} aria-label="补充说明" rows={2} maxLength={2000} value={note} disabled={disabled} onChange={(event) => setNote(event.target.value)} /></Details>
    {check.detail ? <Details><span>{check.detail}</span></Details> : null}
  </ActionCard>
}
