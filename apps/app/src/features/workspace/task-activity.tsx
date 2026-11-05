"use client"

import { useId, useState, type ReactNode } from "react"
import { Link } from "@tanstack/react-router"
import { ChevronRight, CircleQuestionMark, CircleStop, Cpu, Unplug, WifiOff } from "lucide-react"
import { ActionCard, DetailRow, Details, toast } from "@/components/app/kit"
import { BotFace } from "@/components/app/bot-face"
import { Textarea } from "@/components/ui/textarea"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { ActionButton } from "./present"
import type { WorkCheck } from "@vibeface/client/workspace/model.ts"
import type { PendingCheck } from "@vibeface/client/workspace/types.ts"

/** Plain-language reason; the original text stays in the details. */
function checkReason(check: Pick<PendingCheck, "detail">): string {
  if (/restart/i.test(check.detail)) return "设备在 Bot 工作时重启了，系统不会自动重做。"
  if (/stream closed|frame limit|disconnect|connection/i.test(check.detail)) return "和 Bot 的连接断了，系统不会自动重做。"
  return "这次的结果没法自动确认，系统不会自动重做。"
}

export function confirmText(check: Pick<WorkCheck, "reason" | "detail">, device: string): string {
  return check.reason === "offline" ? `${device} 离线了，报不回这个任务的结果。`
    : check.reason === "disconnected" ? `${device} 上的 Codex 断开了，报不回这个任务的结果。`
    : check.detail.startsWith("设备已移除") ? `${device} 已移除，报不回这个任务的结果。`
    : checkReason(check)
}

const confirmIcons = { offline: WifiOff, disconnected: Unplug, unverified: CircleQuestionMark }

/** An offline device reports by itself once back, so waiting also settles it. */
export function BackOnline({ device }: { device: string }) {
  return <p className="text-meta text-muted-foreground">{device} 上线后会自动更新</p>
}

/**
 * "待确认": the person says whether a work its device cannot report finished, or that a leftover process stopped. Nothing is re-run.
 * `inbox`: also names the bot and links to its chat.
 */
export function ConfirmCard({ check, inbox = false, from }: { check: PendingCheck; inbox?: boolean; from?: "inbox" | "tasks" }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const [note, setNote] = useState("")
  const disabled = !workspace.writable || check.kind === "process" && !workspace.selectedBot
  const lookup = { nodes: workspace.nodes, directory }
  if (check.kind === "process") return <ActionCard
    icon={Cpu}
    title="程序还在运行"
    trailing={<ActionButton variant="default" disabled={disabled} action={() => workspace.confirmStopped(check)}><CircleStop aria-hidden="true" />确认已停止</ActionButton>}
  >
    <p className="text-muted-foreground">{checkReason(check)}程序退出后会自动处理。</p>
    <Details>
      <DetailRow label="设备" value={nodeName(workspace.data?.local_node, lookup)} />
      {check.detail ? <span>{check.detail}</span> : null}
    </Details>
  </ActionCard>
  const settle = (work: WorkCheck, choice: "completed" | "interrupted", text = "") => async () => { await workspace.resolveCheck(work, choice, text); toast(choice === "completed" ? "已记为做完" : "已记为没做完", { tone: "success" }) }
  const device = nodeName(check.target_node, lookup)
  const botOf = (work: WorkCheck) => inbox ? workspace.bots.find((item) => item.conversation_id === work.conversation_id) : undefined
  // In the chat this card stands in for the task, so its title opens the task.
  const chatBot = inbox ? undefined : workspace.selectedBot
  const taskLink = (work: WorkCheck, title: ReactNode, className = "") => {
    const bot = botOf(work) ?? chatBot
    return bot ? <Link to="/b/$botId/work/$workId" params={{ botId: bot.bot_id, workId: work.work_id }} search={from === "tasks" ? { from: "tasks" } : from === "inbox" || inbox ? { from: "inbox" } : {}} className={`rounded-sm underline-offset-4 hover:underline ${className}`}>{title}</Link> : title
  }
  const viewTask = (work: WorkCheck) => botOf(work) || chatBot ? <Button asChild variant="outline">{taskLink(work, "查看任务")}</Button> : null
  const settleActions = (work: WorkCheck, text = "") => <Details label="我已自行核对">
    <p className="text-meta text-muted-foreground">只记录你的核对结论，不会重新执行任务。</p>
    <div className="flex flex-wrap justify-end gap-2">
      <ActionButton variant="ghost" disabled={disabled} action={settle(work, "interrupted", text)}>没做完</ActionButton>
      <ActionButton disabled={disabled} action={settle(work, "completed", text)}>做完了</ActionButton>
    </div>
  </Details>
  if (check.kind === "device") return <ActionCard
    icon={confirmIcons[check.reason]}
    title={`待确认 · ${check.works.length} 个任务`}
    meta={device}
  >
    <p className="text-muted-foreground">{check.reason === "offline" ? "设备离线了" : "Codex 断开了"}，暂时收不到这些任务的结果。可以先查看已有记录。</p>
    {check.reason === "offline" ? <BackOnline device={device} /> : null}
    <ul className="divide-y overflow-hidden rounded-lg border bg-card">
      {check.works.map((work) => {
        const bot = botOf(work)
        return <li key={work.work_id} className="grid min-h-13 min-w-0 gap-2 p-3">
          <div className="flex min-w-0 items-center gap-3">
          <span className="grid min-w-0 flex-1">
            <span className="font-medium [overflow-wrap:anywhere]" title={work.title}>{taskLink(work, <span className="line-clamp-2">{work.title || "任务结果"}</span>, "max-md:block max-md:py-2.5")}</span>
            {bot ? <Link to="/b/$botId" params={{ botId: bot.bot_id }} className="inline-flex w-fit max-w-full min-w-0 items-center gap-1 text-meta text-muted-foreground underline-offset-4 hover:text-foreground hover:underline max-md:min-h-11">
              <BotFace bot={bot} mood="idle" size={16} /><span className="sr-only">去对话 </span><span className="truncate">{bot.name}</span></Link> : null}
          </span>
          {viewTask(work)}
          </div>
          {settleActions(work)}
        </li>
      })}
    </ul>
  </ActionCard>
  const reason = confirmText(check, device)
  const bot = botOf(check)
  return <ActionCard
    icon={confirmIcons[check.reason]}
    title={<>待确认 · {taskLink(check, check.title || "任务结果", "max-md:py-3")}</>}
    meta={device}
    actions={viewTask(check)}
  >
    <p>{reason}先查看已有记录，再决定怎么继续。</p>
    {check.reason === "offline" ? <BackOnline device={device} /> : null}
    {bot ? <Link to="/b/$botId" params={{ botId: bot.bot_id }} className="inline-flex w-fit max-w-full min-w-0 items-center gap-1.5 rounded-md text-meta font-medium underline-offset-4 hover:underline max-md:min-h-11">
      <BotFace bot={bot} mood="idle" size={16} /><span className="sr-only">去对话 </span><span className="truncate">{bot.name}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /></Link> : null}
    <Details label="补充说明（可选）"><Textarea id={`${id}-note`} aria-label="补充说明" rows={2} maxLength={2000} value={note} disabled={disabled} onChange={(event) => setNote(event.target.value)} /></Details>
    {settleActions(check, note)}
    {check.detail ? <Details><span>{check.detail}</span></Details> : null}
  </ActionCard>
}
