import { useEffect, useId, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ChevronLeft, Circle, CircleCheck, CircleDot, CircleQuestionMark, CircleStop, FolderOpen, Laptop, Link2, ListChecks, Play, RotateCw, SearchCheck, Settings, X } from "lucide-react"
import { BusyIcon, Details, DetailRow, EmptyState, InlineError, StatusPill, toast, useAction, type Tone } from "@/components/app/kit"
import { Markdown } from "@/components/app/markdown"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { outcomeUnknown, rawError, workFailure } from "@vibeface/client/lib/errors.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { workChecks, type WorkCheck } from "@vibeface/client/workspace/model.ts"
import { BackOnline, confirmText } from "./task-activity"
import { ActionButton } from "./present"
import { QuestionCard } from "./question-card"
import { fixOnDevice, fixOnDeviceLabel, nodeName, progressLabel, shownState, staleTaskLabel, taskActive, taskAgent, taskLabel, taskStale, followUp, followUpCopy, type FollowUp } from "@vibeface/client/workspace/labels.ts"
import { providerNames } from "@vibeface/client/lib/node.ts"
import { providerIcons } from "@/features/devices/ai-settings"
import { CopyDiagnostics } from "@/features/identity/device-controls"
import { projectName, questionOpen } from "@vibeface/client/workspace/questions.ts"
import { folderLabels } from "@vibeface/client/workspace/bot-settings.ts"

const tone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

// The route change unmounts the panel, so focus would otherwise drop to <body>. Module scope keeps the ref stable across renders.
function holdFocus(node: HTMLElement | null) {
  if (!node) return
  const opener = node.ownerDocument.activeElement
  node.focus({ preventScroll: true })
  return () => { if (opener instanceof HTMLElement && opener.isConnected) opener.focus({ preventScroll: true }) }
}

// Below lg the panel covers the chat, so it is a modal; beside the chat it is not. Same query as the lg: classes.
const WIDE = "(min-width: 1024px)"
const followWidth = (change: () => void) => {
  const query = window.matchMedia(WIDE)
  query.addEventListener("change", change)
  return () => query.removeEventListener("change", change)
}

/** Everything outside `keep` stops taking focus and leaves the accessibility tree; live regions such as toasts stay heard. */
function inertAround(keep: Element) {
  const covered: Element[] = []
  for (let node = keep; node.parentElement?.parentElement; node = node.parentElement) {
    for (const sibling of node.parentElement.children) {
      if (sibling === node || sibling.hasAttribute("inert") || sibling.hasAttribute("aria-live")) continue
      sibling.setAttribute("inert", "")
      covered.push(sibling)
    }
  }
  return () => { for (const element of covered) element.removeAttribute("inert") }
}

/**
 * /b/:botId/work/:workId — side panel beside the chat on wide screens, an overlay on tablets, full screen on phones.
 * `from`: opened from 待处理 or 任务, so leaving returns to that list instead of to the chat.
 */
export function TaskDetail({ botId, workId, from }: { botId: string; workId: string; from?: "inbox" | "tasks" }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const action = useAction()
  const lookup = { nodes: workspace.nodes, directory }
  const work = workspace.data?.works.find((item) => item.work_id === workId)
  // A running turn held on an approval reads as 需要你同意, as in the chat, and the question is answered right here.
  const asking = work?.thread_id ? workspace.data?.questions.find((question) => questionOpen(question) && question.about.request_id && question.about.session_id === work.thread_id) : undefined
  const shown = work ? shownState(work, workspace.nodes) : ""
  const state = shown === "running" && asking ? "waiting" : shown
  const [confirming, setConfirming] = useState(false)
  // Its device cannot say whether it finished, so the person settles it here; nothing is re-run.
  const check = work && state === "awaiting_confirmation" ? workChecks([{ ...work, conversation_id: workspace.data?.conversation?.conversation_id ?? "" }], workspace.nodes).find((item): item is WorkCheck => item.kind === "work") : undefined
  const settle = (choice: "completed" | "interrupted") => async () => { if (!check) return; await workspace.resolveCheck(check, choice, ""); toast(choice === "completed" ? "已记为做完" : "已记为没做完", { tone: "success" }) }
  const device = work?.target_node ?? ""
  const agent = work ? taskAgent(work, workspace.nodes) : null
  const AgentIcon = agent ? providerIcons[agent] : null
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && item.work_id === workId && item.target_node === device)
  // Any entry forwards the stop; an unreachable device gets it when it is back.
  const canStop = work && state !== "awaiting_confirmation" && !work.outcome && !work.stop && ["accepted", "running"].includes(work.state)
  const navigate = useNavigate()
  const leave = from === "inbox" ? { to: "/inbox" as const } : from === "tasks" ? { to: "/tasks" as const } : { to: "/b/$botId" as const, params: { botId } }
  const close = () => void navigate(leave)
  const modal = !useSyncExternalStore(followWidth, () => window.matchMedia(WIDE).matches, () => true)
  const titleId = useId()
  const layer = useRef<HTMLDivElement>(null)
  // A layout effect, so the chat is live again before holdFocus returns focus to the task link in it.
  useLayoutEffect(() => modal && layer.current ? inertAround(layer.current) : undefined, [modal])
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape" && !event.defaultPrevented) close() }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [botId, from])
  // The last progress line is stale once a task ends, so it never stands in for a result.
  // A failed task's text is whatever it said before failing, so its cause leads and that text waits in 详细信息.
  // A task settled by hand carries no text either, so an empty result shows no section rather than an odd placeholder.
  const failed = ["failed", "rejected", "dispatch_rejected"].includes(state)
  const cause = work ? workFailure(work) : ""
  const result = failed ? cause || "这个任务没有成功。" : work?.result_text || cause
  // It may have done its writes anyway, so it is checked first; running it again could do them twice.
  const unknown = Boolean(work && (state === "dispatch_unverified" || outcomeUnknown(work.failure, work.failure_reason)))
  const next = followUp(state ?? "", unknown)
  // App Server cannot withdraw one input steered into a turn, so stopping one stops every work sharing it.
  const sharing = work?.turn_id ? (workspace.data?.works ?? []).filter((item) => item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const stop = canStop ? () => workspace.interruptWork(work.work_id) : null

  return <div ref={layer} className="contents">
  {/* Clicking it must not move focus to the page behind, or React hands focus back there instead of to the task link. */}
  <div aria-hidden="true" onMouseDown={(event) => event.preventDefault()} onClick={close} className="fixed inset-0 z-40 hidden bg-black/40 animate-in dark:bg-black/60 fade-in-0 motion-reduce:animate-none md:block lg:hidden" />
  <aside ref={holdFocus} tabIndex={-1} {...modal ? { role: "dialog", "aria-modal": true, "aria-labelledby": titleId, "data-state": "open" } : { "aria-label": "任务详情" }} className="fixed inset-0 z-40 flex flex-col bg-background outline-none md:left-auto md:z-50 md:w-[min(420px,90vw)] md:border-l md:bg-popover md:shadow-xl md:max-lg:animate-in md:max-lg:slide-in-from-right motion-reduce:animate-none lg:static lg:z-auto lg:w-[420px] lg:shrink-0 lg:bg-background lg:shadow-none">
    <header className="flex min-h-14 shrink-0 items-center gap-2 border-b px-4 py-2">
      {/* Full screen on a phone, so it leaves like a page, from the start of its header. */}
      <Button asChild variant="ghost" size="icon" className="-ms-2 md:hidden"><Link {...leave} aria-label="返回" title="返回"><ChevronLeft aria-hidden="true" /></Link></Button>
      <ListChecks aria-hidden="true" className="size-5 shrink-0 text-muted-foreground max-md:hidden" />
      <h2 id={titleId} className="line-clamp-2 min-w-0 flex-1 text-heading font-semibold [overflow-wrap:anywhere]" title={work?.title}>{work?.title || "任务"}</h2>
      {state ? taskStale(state, workspace.connectionOnline) ? <StatusPill>{staleTaskLabel}</StatusPill> : <StatusPill tone={tone(state)}>{taskLabel(state)}</StatusPill> : null}
      <Button asChild variant="ghost" size="icon" className="max-md:hidden"><Link {...leave} aria-label="关闭" title="关闭"><X aria-hidden="true" /></Link></Button>
    </header>
    {!work ? <EmptyState icon={ListChecks} title="找不到这个任务" /> : <div className="grid min-h-0 flex-1 content-start gap-4 overflow-y-auto p-4">
      {device || work.cwd || agent ? <p className="flex min-w-0 items-center gap-x-3 text-meta text-muted-foreground">
        {agent && AgentIcon ? <span className="flex shrink-0 items-center gap-1.5"><AgentIcon aria-hidden="true" className="size-4 shrink-0" />{providerNames[agent]}</span> : null}
        {device ? <span className="flex min-w-0 items-center gap-1.5"><Laptop aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">设备 </span><span className="truncate">{nodeName(device, lookup)}</span></span> : null}
        {work.cwd ? <span className="flex min-w-0 items-center gap-1.5" title={work.cwd}><FolderOpen aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">{folderLabels.project} </span><span className="truncate">{projectName(work.cwd)}</span></span> : null}
      </p> : null}
      {progress?.status && taskActive(state) && progress.status !== "awaiting_input" && progressLabel(progress.status) !== "进行中" ? <p className="text-meta text-muted-foreground">{progressLabel(progress.status)}</p> : null}
      {!progress?.text && work.summary && taskActive(state) ? <Summary text={work.summary} /> : null}
      {progress?.text ? <pre className="max-h-[50dvh] overflow-auto rounded-xl bg-muted p-3 font-mono text-meta whitespace-pre-wrap">{progress.text}</pre> : null}
      {work.prompt ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">交代的事</h3><p className="line-clamp-6 whitespace-pre-wrap break-words" title={work.prompt}>{work.prompt}</p></section> : null}
      {result && !taskActive(state) && !check ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">{failed || !work.result_text ? "原因" : "结果"}</h3>{!failed && work.result_text ? <Markdown text={result} /> : <p className={`whitespace-pre-wrap break-words ${failed ? "text-danger" : ""}`}>{result}</p>}</section> : null}
      {check ? <section className="grid gap-3 rounded-xl border border-warning-border bg-warning-soft p-3">
        <h3 className="flex items-center gap-2 font-medium"><CircleQuestionMark aria-hidden="true" className="size-4 shrink-0 text-warning" />还不知道做完没有</h3>
        <p className="text-meta text-muted-foreground">{confirmText(check, nodeName(device, lookup))}</p>
        {check.reason === "offline" ? <BackOnline device={nodeName(device, lookup)} /> : null}
        <div className="flex flex-wrap justify-end gap-2">
          <ActionButton disabled={!workspace.writable} action={settle("interrupted")}>没做完</ActionButton>
          <ActionButton variant="default" disabled={!workspace.writable} action={settle("completed")}>做完了</ActionButton>
        </div>
      </section> : null}
      {sharing.length && stop ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">会一起停止</h3>
        <ul className="grid gap-1">{sharing.map((item) => <li key={item.work_id} className="flex min-w-0 items-center gap-2"><Link2 aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="truncate">{item.title || "任务"}</span></li>)}</ul></section> : null}
      {asking ? <QuestionCard key={asking.question_id} question={asking} inTask /> : null}
      {stop ? <div className="grid gap-2">
        {confirming || action.busy === "stop" ? <div className="flex flex-wrap items-center gap-2">
          <Button type="button" variant="ghost" disabled={action.busy !== null} onClick={() => setConfirming(false)}>取消</Button>
          <Button type="button" variant="destructive" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("stop", stop).finally(() => setConfirming(false))}><CircleStop aria-hidden="true" />{action.busy === "stop" ? "正在请求停止…" : "确认停止"}</Button>
          <span className="text-meta text-muted-foreground">停下后不会自动接着做</span>
        </div> : <Button type="button" variant="outline" className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => setConfirming(true)}><CircleStop aria-hidden="true" />{sharing.length ? `一起停止 ${sharing.length + 1} 个任务` : "停止任务"}</Button>}
        <InlineError error={action.error} />
      </div> : null}
      {failed || unknown || state === "interrupted" ? <div className="grid gap-2"><div className="flex flex-wrap gap-2">
        {failed && device && fixOnDevice(work) ? <Button asChild variant="outline"><Link to="/devices/$deviceId" params={{ deviceId: device }}><Settings aria-hidden="true" />{fixOnDeviceLabel(nodeName(device, lookup))}</Link></Button> : null}
        {/* After a failure trying again is the next step; a task the person stopped only offers it. */}
        <Button type="button" variant={state === "interrupted" ? "outline" : "default"} className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("retry", async () => {
          if (await workspace.sendMessage(followUpCopy[next].ask(work.title || "这个任务"))) void navigate({ to: "/b/$botId", params: { botId } })
        })}><BusyIcon busy={action.busy === "retry"} icon={followUpIcons[next]} />{followUpCopy[next].action}</Button>
        </div><InlineError error={action.error} />
      </div> : null}
      <Details>
        {failed && work.result_text ? <DetailRow label="已有输出" value={<span className="whitespace-pre-wrap break-words">{work.result_text}</span>} /> : null}
        {work.failure_reason && (check || rawError(work.failure_reason)) ? <DetailRow label={check ? "说明" : "原始错误"} value={<span className="break-words">{work.failure_reason}</span>} /> : null}
        {work.cwd ? <DetailRow label="文件夹" value={work.cwd} mono /> : null}
        <CopyDiagnostics label="复制编号" lines={[["任务编号", work.work_id], ["设备编号", device]]} />
      </Details>
    </div>}
  </aside>
  </div>
}

const followUpIcons: Record<FollowUp, typeof Circle> = { check: SearchCheck, resume: Play, retry: RotateCw }

// A plan's progress arrives as one line per step, marked done, current or to do.
const steps: Record<string, { icon: typeof Circle; className: string; label: string }> = {
  "✓": { icon: CircleCheck, className: "text-success", label: "已完成" },
  "→": { icon: CircleDot, className: "text-info", label: "进行中" },
  "·": { icon: Circle, className: "text-muted-foreground", label: "待做" },
}

/** A task's summary; a plan shows each step with an icon for where it stands. */
function Summary({ text }: { text: string }) {
  const lines = text.split("\n")
  if (!lines.every((line) => steps[line.slice(0, 1)] && line[1] === " ")) return <p className="whitespace-pre-wrap break-words">{text}</p>
  return <ol className="grid gap-1.5">{lines.map((line, index) => {
    const step = steps[line.slice(0, 1)]
    return <li key={index} className={cn("flex min-w-0 items-start gap-2", line.startsWith("·") && "text-muted-foreground")}><step.icon aria-hidden="true" className={cn("mt-0.5 size-4 shrink-0", step.className)} /><span className="sr-only">{step.label} </span><span className="break-words">{line.slice(2)}</span></li>
  })}</ol>
}
