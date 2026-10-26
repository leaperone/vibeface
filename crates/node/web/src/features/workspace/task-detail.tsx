import { useEffect, useId, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ChevronLeft, CircleQuestionMark, CircleStop, Laptop, Link2, ListChecks, RotateCw, Server, X } from "lucide-react"
import { BusyIcon, Details, DetailRow, EmptyState, InlineError, StatusPill, toast, useAction, type Tone } from "@/components/app/kit"
import { Markdown } from "@/components/app/markdown"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { rawError, taskError } from "@vibeface/client/lib/errors.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { workChecks, type WorkCheck } from "@vibeface/client/workspace/model.ts"
import { confirmText } from "./task-activity"
import { ActionButton } from "./present"
import { QuestionCard } from "./question-card"
import { nodeName, progressLabel, shownState, taskActive, taskLabel } from "@vibeface/client/workspace/labels.ts"
import { questionOpen } from "@vibeface/client/workspace/questions.ts"

const tone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation", "stop_timed_out"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

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
 * `from`: opened from 待处理, so leaving goes back there instead of to the chat.
 */
export function TaskDetail({ botId, workId, from }: { botId: string; workId: string; from?: "inbox" }) {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
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
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && item.work_id === workId && item.target_node === device)
  // Any entry forwards the stop; an unreachable device gets it when it is back.
  const canStop = work && state !== "awaiting_confirmation" && !work.outcome && !work.stop && ["accepted", "running"].includes(work.state)
  const navigate = useNavigate()
  const leave = from === "inbox" ? { to: "/inbox" as const } : { to: "/b/$botId" as const, params: { botId } }
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
  // A task settled by hand carries no text either, so an empty result shows no section rather than an odd placeholder.
  const result = work?.result_text || (work?.failure_reason ? taskError(work.failure_reason) : "")
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
      {state ? <StatusPill tone={tone(state)}>{taskLabel(state)}</StatusPill> : null}
      <Button asChild variant="ghost" size="icon" className="max-md:hidden"><Link {...leave} aria-label="关闭" title="关闭"><X aria-hidden="true" /></Link></Button>
    </header>
    {!work ? <EmptyState icon={ListChecks} title="找不到这个任务" /> : <div className="grid min-h-0 flex-1 content-start gap-4 overflow-y-auto p-4">
      {device ? <p className="flex min-w-0 items-center gap-1.5 text-meta text-muted-foreground">{device === ready?.entry.device_id ? <Laptop aria-hidden="true" className="size-4 shrink-0" /> : <Server aria-hidden="true" className="size-4 shrink-0" />}<span className="sr-only">设备 </span><span className="truncate">{nodeName(device, lookup)}</span></p> : null}
      {progress?.status && taskActive(state) && progress.status !== "awaiting_input" && progressLabel(progress.status) !== "进行中" ? <p className="text-meta text-muted-foreground">{progressLabel(progress.status)}</p> : null}
      {!progress?.text && work.summary && taskActive(state) ? <p className="whitespace-pre-wrap break-words">{work.summary}</p> : null}
      {progress?.text ? <pre className="max-h-[50dvh] overflow-auto rounded-xl bg-muted p-3 font-mono text-meta whitespace-pre-wrap">{progress.text}</pre> : null}
      {result && !taskActive(state) && !check ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">{work.failure_reason && !work.result_text ? "原因" : "结果"}</h3>{work.result_text ? <Markdown text={result} /> : <p className="whitespace-pre-wrap break-words">{result}</p>}</section> : null}
      {check ? <section className="grid gap-3 rounded-xl border border-warning-border bg-warning-soft p-3">
        <h3 className="flex items-center gap-2 font-medium"><CircleQuestionMark aria-hidden="true" className="size-4 shrink-0 text-warning" />还不知道做完没有</h3>
        <p className="text-meta text-muted-foreground">{confirmText(check, nodeName(device, lookup))}</p>
        <div className="flex flex-wrap justify-end gap-2">
          <ActionButton disabled={!workspace.writable} action={settle("interrupted")}>没做完</ActionButton>
          <ActionButton variant="default" disabled={!workspace.writable} action={settle("completed")}>做完了</ActionButton>
        </div>
      </section> : null}
      {sharing.length && stop ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">会一起停止</h3>
        <ul className="grid gap-1">{sharing.map((item) => <li key={item.work_id} className="flex min-w-0 items-center gap-2"><Link2 aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="truncate">{item.title || "任务"}</span></li>)}</ul></section> : null}
      {asking ? <QuestionCard question={asking} inTask /> : null}
      {stop ? <div className="grid gap-2">
        {confirming || action.busy === "stop" ? <div className="flex flex-wrap items-center gap-2">
          <Button type="button" variant="ghost" disabled={action.busy !== null} onClick={() => setConfirming(false)}>取消</Button>
          <Button type="button" variant="destructive" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("stop", stop).finally(() => setConfirming(false))}><CircleStop aria-hidden="true" />{action.busy === "stop" ? "正在请求停止…" : "确认停止"}</Button>
          <span className="text-meta text-muted-foreground">停下后不会自动接着做</span>
        </div> : <Button type="button" variant="outline" className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => setConfirming(true)}><CircleStop aria-hidden="true" />{sharing.length ? `一起停止 ${sharing.length + 1} 个任务` : "停止任务"}</Button>}
        <InlineError error={action.error} />
      </div> : null}
      {["failed", "interrupted", "rejected"].includes(state) ? <div className="grid gap-2">
        {/* After a failure trying again is the next step; a task the person stopped only offers it. */}
        <Button type="button" variant={state === "interrupted" ? "outline" : "default"} className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("retry", async () => {
          if (await workspace.sendMessage(`再试一次「${work.title || "这个任务"}」`)) void navigate({ to: "/b/$botId", params: { botId } })
        })}><BusyIcon busy={action.busy === "retry"} icon={RotateCw} />让 Bot 再试</Button>
        <InlineError error={action.error} />
      </div> : null}
      <Details>
        {work.prompt ? <DetailRow label="交代的事" value={<span className="whitespace-pre-wrap break-words">{work.prompt}</span>} /> : null}
        {work.failure_reason && (check || rawError(work.failure_reason)) ? <DetailRow label={check ? "说明" : "原始错误"} value={<span className="break-words">{work.failure_reason}</span>} /> : null}
        <DetailRow label="任务编号" value={work.work_id} mono />
        {work.cwd ? <DetailRow label="文件夹" value={work.cwd} mono /> : null}
        {device ? <DetailRow label="设备编号" value={device} mono /> : null}
      </Details>
    </div>}
  </aside>
  </div>
}
