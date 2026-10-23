import { useEffect, useState } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { Laptop, Link2, ListChecks, RotateCw, Server, Square, X } from "lucide-react"
import { BusyIcon, Details, DetailRow, EmptyState, InlineError, StatusPill, useAction, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import { humanError, rawError } from "@/lib/errors"
import { useWorkspace } from "./context"
import { confirmReason } from "./model"
import { confirmText } from "./task-activity"
import { QuestionCard } from "./question-card"
import { nodeName, progressLabel, shownState, taskActive, taskLabel } from "./labels"

const tone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation", "stop_timed_out"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** /b/:botId/work/:workId — desktop side panel beside the chat, full screen on phones. */
export function TaskDetail({ botId, workId }: { botId: string; workId: string }) {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const action = useAction()
  const lookup = { nodes: workspace.nodes, directory }
  const work = workspace.data?.works.find((item) => item.work_id === workId)
  // A running turn held on an approval reads as 需要你同意, as in the chat, and the question is answered right here.
  const asking = work?.thread_id ? workspace.data?.questions.find((question) => !question.answer && question.about.request_id && question.about.session_id === work.thread_id) : undefined
  const shown = work ? shownState(work, workspace.nodes) : ""
  const state = shown === "running" && asking ? "waiting" : shown
  const [confirming, setConfirming] = useState(false)
  const device = work?.target_node ?? ""
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && item.work_id === workId && item.target_node === device)
  // Any entry forwards the stop; an unreachable device gets it when it is back.
  const canStop = work && state !== "awaiting_confirmation" && !work.outcome && !work.stop && ["accepted", "running"].includes(work.state)
  const navigate = useNavigate()
  const close = () => void navigate({ to: "/b/$botId", params: { botId } })
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape" && !event.defaultPrevented) close() }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [botId])
  // The last progress line is stale once a task ends, so it never stands in for a result.
  const result = work?.result_text || (work?.failure_reason ? humanError(work.failure_reason, undefined, "这个任务") : state === "completed" ? "没有返回文字" : "")
  // App Server cannot withdraw one input steered into a turn, so stopping one stops every work sharing it.
  const sharing = work?.turn_id ? (workspace.data?.works ?? []).filter((item) => item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const stop = canStop ? () => workspace.interruptWork(work.work_id) : null

  return <aside aria-label="任务详情" className="fixed inset-0 z-40 flex flex-col bg-background md:static md:z-auto md:w-[420px] md:shrink-0 md:border-l">
    <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
      <ListChecks aria-hidden="true" className="size-5 text-muted-foreground" />
      <h2 className="min-w-0 flex-1 truncate text-heading font-semibold">{work?.title || "任务"}</h2>
      {state ? <StatusPill tone={tone(state)}>{taskLabel(state)}</StatusPill> : null}
      <Button asChild variant="ghost" size="icon"><Link to="/b/$botId" params={{ botId }} aria-label="关闭" title="关闭"><X aria-hidden="true" /></Link></Button>
    </header>
    {!work ? <EmptyState icon={ListChecks} title="找不到这个任务" /> : <div className="grid min-h-0 flex-1 content-start gap-4 overflow-y-auto p-4">
      {device ? <p className="flex min-w-0 items-center gap-1.5 text-meta text-muted-foreground">{device === ready?.entry.device_id ? <Laptop aria-hidden="true" className="size-4 shrink-0" /> : <Server aria-hidden="true" className="size-4 shrink-0" />}<span className="sr-only">设备 </span><span className="truncate">{nodeName(device, lookup)}</span></p> : null}
      {progress?.status && taskActive(state) && progress.status !== "awaiting_input" && progressLabel(progress.status) !== "进行中" ? <p className="text-meta text-muted-foreground">{progressLabel(progress.status)}</p> : null}
      {!progress?.text && work.summary && taskActive(state) ? <p className="whitespace-pre-wrap break-words">{work.summary}</p> : null}
      {progress?.text ? <pre className="max-h-[50dvh] overflow-auto rounded-xl bg-muted p-3 font-mono text-meta whitespace-pre-wrap">{progress.text}</pre> : null}
      {result && !taskActive(state) ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">结果</h3><p className="whitespace-pre-wrap break-words">{result}</p></section> : null}
      {state === "awaiting_confirmation" ? <p className="text-meta text-muted-foreground">{confirmText({ reason: confirmReason(work, workspace.nodes) ?? "unverified", detail: work.failure_reason ?? "" }, nodeName(device, lookup))}请在对话或待处理里确认它做完了没有。</p> : null}
      {sharing.length && stop ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">会一起停止</h3>
        <ul className="grid gap-1">{sharing.map((item) => <li key={item.work_id} className="flex min-w-0 items-center gap-2"><Link2 aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="truncate">{item.title || "任务"}</span></li>)}</ul></section> : null}
      {asking ? <QuestionCard question={asking} /> : null}
      {stop ? <div className="grid gap-2">
        {confirming || action.busy === "stop" ? <div className="flex flex-wrap items-center gap-2">
          <Button type="button" variant="destructive" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("stop", stop).finally(() => setConfirming(false))}><Square aria-hidden="true" />{action.busy === "stop" ? "正在请求停止…" : "确认停止"}</Button>
          <Button type="button" variant="ghost" disabled={action.busy !== null} onClick={() => setConfirming(false)}>取消</Button>
          <span className="text-meta text-muted-foreground">停下后不会自动接着做</span>
        </div> : <Button type="button" variant="outline" className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => setConfirming(true)}><Square aria-hidden="true" />{sharing.length ? `一起停止 ${sharing.length + 1} 个任务` : "停止任务"}</Button>}
        <InlineError error={action.error} />
      </div> : null}
      {["failed", "interrupted", "rejected"].includes(state) ? <div className="grid gap-2">
        <Button type="button" variant="outline" className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("retry", async () => {
          if (await workspace.sendMessage(`再试一次「${work.title || "这个任务"}」`)) close()
        })}><BusyIcon busy={action.busy === "retry"} icon={RotateCw} />让 Bot 再试</Button>
        <InlineError error={action.error} />
      </div> : null}
      <Details>
        {work.prompt ? <DetailRow label="交代的事" value={<span className="whitespace-pre-wrap break-words">{work.prompt}</span>} /> : null}
        {work.failure_reason && rawError(work.failure_reason) ? <DetailRow label="原始错误" value={<span className="break-words">{work.failure_reason}</span>} /> : null}
        <DetailRow label="任务编号" value={work.work_id} mono />
        {work.cwd ? <DetailRow label="文件夹" value={work.cwd} mono /> : null}
        {device ? <DetailRow label="设备编号" value={device} mono /> : null}
      </Details>
    </div>}
  </aside>
}
