import { useEffect, useRef } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { CircleCheck, CircleX, ListChecks, Square, X } from "lucide-react"
import { Details, DetailRow, EmptyState, InlineError, StatusPill, useAction, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import { useWorkspace } from "./context"
import { approvalKind, decisionLabel, nodeName, progressLabel, shownState, taskActive, taskLabel } from "./labels"
import { ApprovalCard } from "./task-activity"

const tone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["unverified", "waiting_approval", "stop_unconfirmed"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** /b/:botId/work/:workId — desktop side panel beside the chat, full screen on phones. */
export function TaskDetail({ botId, workId }: { botId: string; workId: string }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const action = useAction()
  const lookup = { nodes: workspace.nodes, directory }
  const work = workspace.data?.works.find((item) => item.work_id === workId)
  const state = work ? shownState(work) : ""
  const device = work?.target_node ?? ""
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && item.work_id === workId && item.target_node === device)
  // Any entry forwards the stop; an unreachable device gets it when it is back.
  const canStop = work && !work.outcome && !work.stop && ["accepted", "starting", "running", "waiting_approval"].includes(work.state ?? "")
  const navigate = useNavigate()
  const close = () => void navigate({ to: "/b/$botId", params: { botId } })
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape" && !event.defaultPrevented) close() }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [botId])
  // A new approval anywhere in this conversation takes over the panel, so it is never hidden behind an older task.
  const seen = useRef<Set<string> | null>(null)
  const waiting = (workspace.data?.works ?? []).flatMap((one) => one.open_approvals.map((request) => ({ key: `${one.work_id}:${request.request_id}`, work: one.work_id })))
  useEffect(() => {
    const fresh = seen.current && waiting.find((item) => !seen.current!.has(item.key))
    seen.current = new Set(waiting.map((item) => item.key))
    if (fresh && fresh.work !== workId) void navigate({ to: "/b/$botId/work/$workId", params: { botId, workId: fresh.work } })
  }, [waiting.map((item) => item.key).join()])
  const answered = work?.answered_approvals ?? []
  const result = work?.result_text || work?.failure_reason || work?.summary || ""
  const stop = canStop ? () => workspace.interruptWork(work.work_id) : null

  return <aside aria-label="任务详情" className="fixed inset-0 z-40 flex flex-col bg-background md:static md:z-auto md:w-[420px] md:shrink-0 md:border-l">
    <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
      <ListChecks aria-hidden="true" className="size-5 text-muted-foreground" />
      <h2 className="min-w-0 flex-1 truncate text-heading font-semibold">任务{device ? ` · ${nodeName(device, lookup)}` : ""}</h2>
      {state ? <StatusPill tone={tone(state)}>{taskLabel(state)}</StatusPill> : null}
      <Button asChild variant="ghost" size="icon"><Link to="/b/$botId" params={{ botId }} aria-label="关闭" title="关闭"><X aria-hidden="true" /></Link></Button>
    </header>
    {!work ? <EmptyState icon={ListChecks} title="找不到这个任务" /> : <div className="grid min-h-0 flex-1 content-start gap-4 overflow-y-auto p-4">
      <p className="whitespace-pre-wrap break-words">{work.prompt}</p>
      {progress?.status && taskActive(state) && state !== "waiting_approval" ? <p className="text-meta text-muted-foreground">{progressLabel(progress.status)}</p> : null}
      {progress?.text ? <pre className="max-h-[50dvh] overflow-auto rounded-xl bg-muted p-3 font-mono text-meta whitespace-pre-wrap">{progress.text}</pre> : null}
      {work.open_approvals.map((request) => <ApprovalCard key={request.request_id} work={work} request={request} stoppable={false} />)}
      {result && !taskActive(state) ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">结果</h3><p className="whitespace-pre-wrap break-words">{result}</p></section> : null}
      {answered.length ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">你处理过的</h3>
        {answered.map((answer) => <p key={answer.request_id} className="flex min-w-0 items-center gap-2 text-meta">
          {answer.decision === "accept" || answer.decision === "answered" ? <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" /> : <CircleX aria-hidden="true" className="size-4 shrink-0 text-danger" />}
          <span className="shrink-0">{decisionLabel(answer.decision)} · {approvalKind(answer)}</span>
        </p>)}
      </section> : null}
      {stop ? <div className="grid gap-2">
        <Button type="button" variant="destructive" className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("stop", stop)}><Square aria-hidden="true" />{action.busy === "stop" ? "正在请求停止…" : "停止任务"}</Button>
        <InlineError error={action.error} />
      </div> : null}
      <Details>
        <DetailRow label="任务编号" value={work.work_id} mono />
        {work.cwd ? <DetailRow label="文件夹" value={work.cwd} mono /> : null}
        {device ? <DetailRow label="设备编号" value={device} mono /> : null}
      </Details>
    </div>}
  </aside>
}
