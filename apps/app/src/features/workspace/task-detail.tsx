import { useEffect, useId, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { Bot, ChevronLeft, Circle, CircleCheck, CircleDot, CircleQuestionMark, CircleStop, FolderOpen, Laptop, Link2, ListChecks, Play, RotateCw, SearchCheck, Settings, X } from "lucide-react"
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
import { agentLaneLabels, fixOnDevice, fixOnDeviceLabel, nodeName, parseSummarySteps, progressLabel, promptSummary, queueHoldLabels, schedulerStatus, shownState, staleTaskLabel, taskActive, taskAgent, taskLabel, taskProgress, taskStale, followUp, followUpCopy, type FollowUp, type SummaryStep } from "@vibeface/client/workspace/labels.ts"
import { providerNames } from "@vibeface/client/lib/node.ts"
import { providerIcons } from "@/features/devices/ai-settings"
import { CopyDiagnostics } from "@/features/identity/device-controls"
import { projectName, workApproval } from "@vibeface/client/workspace/questions.ts"
import { folderLabels } from "@vibeface/client/workspace/bot-settings.ts"
import type { WorkDetail } from '@vibeface/client/workspace/types.ts'
import { PromptInput } from "@/features/chat/elements/prompt-input"
import { InputQueue, WorkspaceInputStatus } from "./workspace-chat"
import { promptIntent } from "@vibeface/client/chat/protocol.ts"

const tone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"
type ComposerDraft = { text: string; files: File[]; workId: string; conversationId: string }

// The route change unmounts the panel, so focus would otherwise drop to <body>. Module scope keeps the ref stable across renders.
function holdFocus(node: HTMLElement | null) {
  if (!node) return
  const opener = node.ownerDocument.activeElement
  node.focus({ preventScroll: true })
  return () => {
    const target = opener instanceof HTMLElement && opener !== node.ownerDocument.body && opener.isConnected ? opener : node.ownerDocument.getElementById("main")
    target?.focus({ preventScroll: true })
  }
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
  const routeActive = useRef(true)
  useEffect(() => { routeActive.current = true; return () => { routeActive.current = false } }, [])
  const lookup = { nodes: workspace.nodes, directory }
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const conversationId = bot?.conversation_id
  const [detail, setDetail] = useState<{ generation: number; value: WorkDetail } | null>(null)
  const [readError, setReadError] = useState<{ generation: number; conversationId: string; workId: string; cause: unknown } | null>(null)
  const [retry, setRetry] = useState(0)
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => {
    const current = latest.current
    if (!conversationId || !current.connectionOnline) return
    let alive = true
    setReadError(null)
    void current.readWork({ conversation_id: conversationId, work_id: workId }).then((value) => {
      if (alive && current.isCurrent()) setDetail({ generation: current.generation, value })
    }, (cause: unknown) => {
      if (alive && current.isCurrent()) setReadError({ generation: current.generation, conversationId, workId, cause })
    })
    return () => { alive = false }
  }, [conversationId, workId, workspace.generation, workspace.connectionOnline, workspace.data, workspace.openWorks, retry])
  const loaded = detail?.generation === workspace.generation && detail.value.work.conversation_id === conversationId && detail.value.work.work_id === workId ? detail.value : null
  const readFailure = readError?.generation === workspace.generation && readError.conversationId === conversationId && readError.workId === workId ? readError.cause : null
  const work = loaded ? { ...loaded.work, target: loaded.work.target ?? undefined } : undefined
  // A running turn held on an approval reads as 需要你同意, as in the chat, and the question is answered right here.
  const asking = work ? workApproval({ work, conversationId: work.conversation_id, questions: workspace.questions, answered: workspace.answered }) : undefined
  const shown = work ? shownState(work, workspace.nodes) : ""
  const state = shown === "running" && asking ? "waiting" : shown
  const [confirming, setConfirming] = useState(false)
  const [input, setInput] = useState('')
  const [files, setFiles] = useState<File[]>([])
  const [sendError, setSendError] = useState<unknown>(null)
  const draftRef = useRef<ComposerDraft>({ text: input, files, workId, conversationId: conversationId ?? '' })
  if (draftRef.current.text !== input || draftRef.current.files !== files || draftRef.current.workId !== workId || draftRef.current.conversationId !== (conversationId ?? '')) draftRef.current = { text: input, files, workId, conversationId: conversationId ?? '' }
  const draft = draftRef.current
  const savingDrafts = useRef(new Set<ComposerDraft>())
  // Its device cannot say whether it finished, so the person settles it here; nothing is re-run.
  const check = work && state === "awaiting_confirmation" ? workChecks([work], workspace.nodes).find((item): item is WorkCheck => item.kind === "work") : undefined
  const settle = (choice: "completed" | "interrupted") => async () => { if (!check) return; await workspace.resolveCheck(check, choice, ""); toast(choice === "completed" ? "已记为做完" : "已记为没做完", { tone: "success" }) }
  const device = work?.target_node ?? ""
  const agent = work ? taskAgent(work) : null
  const AgentIcon = agent ? providerIcons[agent] : null
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && item.work_id === workId && item.target_node === device)
  const progressText = progress?.text?.trim() ?? ""
  const stale = Boolean(state && taskStale(state, workspace.connectionOnline))
  const plan = parseSummarySteps(work?.summary ?? "")
  const conciseProgress = plan ? plan.find((step) => step.state === "current")?.text ?? ""
    : work ? taskProgress(work, workspace.liveProgress).trim().split(/\r?\n/).filter((line) => line.trim()).at(-1) ?? "" : ""
  const progressStatus = progress?.status && progress.status !== "awaiting_input" ? progressLabel(progress.status) : ""
  const progressHeadline = taskActive(state) ? conciseProgress || (progressStatus !== "进行中" ? progressStatus : "") : ""
  const navigate = useNavigate()
  const checkResult = async () => {
    await workspace.refresh()
    setRetry((value) => value + 1)
  }
  const canAskCheck = workspace.writable && workspace.connectionOnline && !workspace.incomplete && check?.reason !== "offline"
  // Any entry forwards the stop; an unreachable device gets it when it is back.
  const canStop = work && state !== "awaiting_confirmation" && !work.outcome && !work.stop && ["accepted", "running"].includes(work.state)
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
  const taskTarget = work?.target?.kind === 'task' || work?.target?.kind === 'channel_task' ? work.target : null
  const followUpLabel = unknown ? followUpCopy[next].action : taskTarget ? '继续所属任务' : state === 'interrupted' ? '新任务继续' : '新任务重做'
  // App Server cannot withdraw one input steered into a turn, so stopping one stops every work sharing it.
  const sharing = work?.turn_id ? workspace.openWorks.filter((item) => item.conversation_id === work.conversation_id && item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const stop = canStop ? () => workspace.interruptWork(work.work_id) : null
  const asked = promptSummary(work?.prompt ?? "", work?.title ?? "")
  const coordination = work?.coordination_job
  const canContinue = Boolean(work && !work.outcome && !work.stop && ["accepted", "running"].includes(work.state))
  // A work whose result is unknown may still write; new input waits until the result is checked.
  const inputPaused = !canContinue && (!work?.outcome || unknown || work?.outcome === "unverified")
  const inputIntent = canContinue ? promptIntent(input) : "new_task"

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
    {!work ? <div className="grid gap-3 p-4"><EmptyState icon={ListChecks} title={!bot ? '找不到这个 Bot' : !workspace.connectionOnline ? '工作状态未知，连接后再读取' : readFailure ? '未能读取这个工作' : '正在读取工作'} /><InlineError error={readFailure} />{bot ? <Button type="button" variant="outline" className="justify-self-start" disabled={!workspace.connectionOnline} onClick={() => setRetry((value) => value + 1)}>重新读取</Button> : null}</div> : <div className="grid min-h-0 flex-1 content-start gap-4 overflow-y-auto p-4">
      {bot ? <Link to="/b/$botId" params={{ botId }} className="flex min-h-11 w-fit items-center gap-1.5 rounded-md text-meta font-medium underline-offset-4 hover:underline"><Bot aria-hidden="true" className="size-4" /><span className="sr-only">所属 Bot </span>{bot.name}</Link> : null}
      <div className="grid gap-1"><p className="text-meta text-muted-foreground">来源：共享历史中的工作记录。完整执行历史未提供。</p><Button type="button" variant="ghost" size="sm" className="justify-self-start" disabled={!workspace.connectionOnline} onClick={() => setRetry((value) => value + 1)}>重新读取工作</Button></div>
      {!workspace.connectionOnline ? <p role="status" className="text-meta text-warning">连接已断开，显示最近记录；当前状态未知。</p> : null}
      <InlineError error={readFailure} action={{ label: '重新读取', onClick: () => setRetry((value) => value + 1) }} />
      {device || work.cwd || agent ? <p className="flex min-w-0 items-center gap-x-3 text-meta text-muted-foreground">
        {agent && AgentIcon ? <span className="flex shrink-0 items-center gap-1.5"><AgentIcon aria-hidden="true" className="size-4 shrink-0" />{providerNames[agent]}</span> : null}
        {device ? <span className="flex min-w-0 items-center gap-1.5"><Laptop aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">设备 </span><span className="truncate">{nodeName(device, lookup)}</span></span> : null}
        {work.cwd ? <span className="flex min-w-0 items-center gap-1.5" title={work.cwd}><FolderOpen aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">{folderLabels.project} </span><span className="truncate">{projectName(work.cwd)}</span></span> : null}
      </p> : null}
      {progressHeadline ? <p className="line-clamp-2 break-words text-meta text-muted-foreground">{stale ? '最近进展：' : ''}{progressHeadline}</p> : null}
      {work.kind === 'task' ? <Details label="协调回合">
        {coordination ? <>
          <p>{!workspace.connectionOnline || workspace.nodes.find((node) => node.device_id === device)?.online === false ? '当前状态未知，显示最近记录' : schedulerStatus(coordination.state).label}{coordination.lane ? ` · ${agentLaneLabels[coordination.lane]}` : ''}</p>
          {coordination.hold_reason ? <p>{queueHoldLabels[coordination.hold_reason]}</p> : null}
        </> : <p>执行设备尚未提供这个任务的协调回合观察。</p>}
        <p className="text-meta text-muted-foreground">这里显示 Bot 的准备和排队情况，任务结果仍以上面的共享记录为准。</p>
      </Details> : null}
      {plan ? <Details label={`最近步骤摘要 · ${plan.length}`} open={taskActive(state)}><SummarySteps steps={plan} /></Details> : null}
      {taskActive(state) && (progressText || !plan && work.summary) ? <Details label="详细进展"><p className="max-h-[50dvh] overflow-auto whitespace-pre-wrap break-words text-meta">{progressText || work.summary || ""}</p></Details> : null}
      {asked.summary ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">交代的事</h3><p className="break-words">{asked.summary}</p></section> : null}
      {result && !taskActive(state) && !check ? <section className="grid gap-1"><h3 className="text-meta font-medium text-muted-foreground">{failed || !work.result_text ? "原因" : "结果"}</h3>{!failed && work.result_text ? <Markdown text={result} /> : <p className={`whitespace-pre-wrap break-words ${failed ? "text-danger" : ""}`}>{result}</p>}</section> : null}
      {check ? <section className="grid gap-3 rounded-xl border border-warning-border bg-warning-soft p-3">
        <h3 className="flex items-center gap-2 font-medium"><CircleQuestionMark aria-hidden="true" className="size-4 shrink-0 text-warning" />还不知道做完没有</h3>
        <p className="text-meta text-muted-foreground">{confirmText(check, nodeName(device, lookup))}</p>
        {check.reason === "offline" ? <BackOnline device={nodeName(device, lookup)} /> : null}
        <ActionButton variant="default" className="w-fit justify-self-end" disabled={!canAskCheck || action.busy !== null} action={checkResult}><SearchCheck aria-hidden="true" />先查结果</ActionButton>
        <Details label="我已自行核对">
          <p className="text-meta text-muted-foreground">只记录你的核对结论，不会重新执行任务。</p>
          <div className="flex flex-wrap justify-end gap-2">
            <ActionButton disabled={!workspace.writable} action={settle("interrupted")}>没做完</ActionButton>
            <ActionButton variant="default" disabled={!workspace.writable} action={settle("completed")}>做完了</ActionButton>
          </div>
        </Details>
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
      {(!check && (failed || unknown || state === "interrupted")) ? <div className="grid gap-2"><div className="flex flex-wrap gap-2">
        {failed && device && fixOnDevice(work) ? <Button asChild variant="outline"><Link to="/devices/$deviceId" params={{ deviceId: device }}><Settings aria-hidden="true" />{fixOnDeviceLabel(nodeName(device, lookup))}</Link></Button> : null}
        {/* After a failure trying again is the next step; a task the person stopped only offers it. */}
        <Button type="button" variant={state === "interrupted" ? "outline" : "default"} className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("retry", async () => {
          if (unknown) { await checkResult(); return }
          let revision: number | undefined
          if (taskTarget) {
            const state = await workspace.readWorkspaceState({ task_id: taskTarget.task_id })
            const task = state.tasks.find((item) => item.task_id === taskTarget.task_id)
            if (!task) throw new Error('尚未读到所属任务，请先查询任务。')
            if (task.action !== 'continue') { await navigate({ to: '/tasks/$taskId', params: { taskId: task.task_id } }); return }
            revision = task.revision
          }
          const request = `${followUpCopy[next].ask(work.title || '这项工作')}\n\n原交代：\n${work.prompt}${work.result_text ? `\n\n上次已有输出：\n${work.result_text}` : ''}`
          await workspace.sendMessage(request, [], undefined, { target: taskTarget ?? { kind: 'work', work_id: work.work_id }, intent: taskTarget ? 'continue' : 'new_task', ...(revision === undefined ? {} : { expected_revision: revision }), conversation_id: work.conversation_id, onSaved: (saved) => { if (!routeActive.current || !workspace.isCurrent()) return; if (saved.target.kind === 'task' || saved.target.kind === 'channel_task') void navigate({ to: '/tasks/$taskId', params: { taskId: saved.target.task_id } }) } })
        })}><BusyIcon busy={action.busy === "retry"} icon={followUpIcons[next]} />{followUpLabel}</Button>
        </div><InlineError error={action.error} />
      </div> : null}
      <Details>
        {asked.folded ? <DetailRow label="交代的原文" value={<span className="whitespace-pre-wrap break-words">{work.prompt}</span>} /> : null}
        {failed && work.result_text ? <DetailRow label="已有输出" value={<span className="whitespace-pre-wrap break-words">{work.result_text}</span>} /> : null}
        {work.failure_reason && (check || rawError(work.failure_reason)) ? <DetailRow label={check ? "说明" : "原始错误"} value={<span className="break-words">{work.failure_reason}</span>} /> : null}
        {work.cwd ? <DetailRow label="文件夹" value={work.cwd} mono /> : null}
        <CopyDiagnostics label="复制编号" lines={[["任务编号", work.work_id], ["设备编号", device]]} />
      </Details>
      {workspace.pendingMessages.filter((item) => item.target.kind === 'work' && item.target.work_id === workId).map((item) => <WorkspaceInputStatus key={item.message_id} pending={item} />)}
    </div>}
    {work ? <div className="shrink-0 border-t p-3"><PromptInput value={input} onChange={setInput} files={files} onFiles={setFiles} hint="" reason={inputPaused ? (unknown || work.outcome === "unverified" ? "先核对这个工作的结果，再继续。" : "这个工作当前不能接收补充。") : workspace.readOnlyNotice || workspace.writeNotice} disabled={!workspace.canSaveMessages || inputPaused} label={inputIntent === "continue" ? `补充给 ${work.title || '这个工作'}` : '发送为新任务'} sendLabel={inputIntent === "continue" ? "补充这个工作" : "发送为新任务"} target={<><span className="text-meta text-muted-foreground">{canContinue ? '补充给' : '新任务 · 接着'} · {work.title || '这个工作'}</span><InputQueue target={{ kind: "work", work_id: workId }} /></>} onSubmit={() => {
      const submittedDraft = draft
      if (savingDrafts.current.has(submittedDraft)) return
      savingDrafts.current.add(submittedDraft)
      const text = input, selectedFiles = files
      setSendError(null)
      void workspace.sendMessage(text, selectedFiles, undefined, { target: { kind: 'work', work_id: workId }, intent: inputIntent, conversation_id: work.conversation_id, onSaved: (saved) => { if (!routeActive.current || !workspace.isCurrent() || draftRef.current !== submittedDraft) return; if (saved.target.kind === 'task' || saved.target.kind === 'channel_task') void navigate({ to: '/tasks/$taskId', params: { taskId: saved.target.task_id } }) } }).then((saved) => { if (saved && draftRef.current === submittedDraft) { setInput((current) => current === text ? '' : current); setFiles((current) => current === selectedFiles ? [] : current) } }).catch(setSendError).finally(() => { savingDrafts.current.delete(submittedDraft) })
    }}><InlineError error={sendError} /></PromptInput></div> : null}
  </aside>
  </div>
}

const followUpIcons: Record<FollowUp, typeof Circle> = { check: SearchCheck, resume: Play, retry: RotateCw }

const steps: Record<SummaryStep["state"], { icon: typeof Circle; className: string; label: string }> = {
  done: { icon: CircleCheck, className: "text-success", label: "已完成" },
  current: { icon: CircleDot, className: "text-info", label: "进行中" },
  pending: { icon: Circle, className: "text-muted-foreground", label: "待做" },
}

function SummarySteps({ steps: items }: { steps: SummaryStep[] }) {
  return <ol className="grid gap-1.5">{items.map((item, index) => {
    const step = steps[item.state]
    return <li key={`${item.state}:${index}`} className={cn("flex min-w-0 items-start gap-2", item.state === "pending" && "text-muted-foreground")}><step.icon aria-hidden="true" className={cn("mt-0.5 size-4 shrink-0", step.className)} /><span className="sr-only">{step.label} </span><span className="break-words">{item.text}</span></li>
  })}</ol>
}
