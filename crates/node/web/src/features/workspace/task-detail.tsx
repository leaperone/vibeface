import { Link } from "@tanstack/react-router"
import { ListChecks, Square, X } from "lucide-react"
import { Details, DetailRow, EmptyState, InlineError, StatusPill, useAction, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import { useWorkspace } from "./context"
import { nodeName, taskActive, taskLabel } from "./labels"
import { NativeSessionActivity } from "./task-activity"

const tone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["pending_check", "unknown", "waiting"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** /b/:botId/work/:workId — desktop side panel beside the chat, full screen on phones. */
export function TaskDetail({ botId, workId }: { botId: string; workId: string }) {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const action = useAction()
  const lookup = { nodes: workspace.nodes, directory }
  const task = workspace.data?.agent_tasks.find((item) => item.task_ref === workId)
  const sessions = workspace.data?.native_sessions.filter((item) => item.work_id === workId || task && item.task_ref === task.task_ref) ?? []
  const session = sessions[0]
  const state = task?.state ?? session?.state ?? ""
  const device = task?.target_node ?? session?.target_node ?? ""
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && sessions.some((one) => one.work_id === item.work_id && one.target_node === item.target_node))
  const canCancelTask = task && ["pending_delivery", "queued", "claimed", "accepted", "running"].includes(task.state) && (ready?.entry.peer_id === task.source_node || ready?.entry.peer_id === task.target_node)
  const canStopSession = !task && session?.turn_id && ["running", "waiting"].includes(session.state) && session.can_control && session.freshness === "observed"
  const stop = canCancelTask ? () => workspace.cancelAgentTask(task.task_ref) : canStopSession ? () => workspace.interruptNative(session.work_id, session.target_node) : null

  return <aside aria-label="任务详情" className="fixed inset-0 z-40 flex flex-col bg-background md:static md:z-auto md:w-[420px] md:shrink-0 md:border-l">
    <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
      <ListChecks aria-hidden="true" className="size-5 text-muted-foreground" />
      <h2 className="min-w-0 flex-1 truncate text-heading font-semibold">任务{device ? ` · ${nodeName(device, lookup)}` : ""}</h2>
      {state ? <StatusPill tone={tone(state)}>{taskLabel(state)}</StatusPill> : null}
      <Button asChild variant="ghost" size="icon"><Link to="/b/$botId" params={{ botId }} aria-label="关闭" title="关闭"><X aria-hidden="true" /></Link></Button>
    </header>
    {!task && !session ? <EmptyState icon={ListChecks} title="找不到这个任务" /> : <div className="grid min-h-0 flex-1 content-start gap-4 overflow-y-auto p-4">
      {task?.text ? <p className="whitespace-pre-wrap break-words">{task.text}</p> : null}
      {progress?.status ? <p className="text-meta text-muted-foreground">{progress.status}</p> : null}
      {progress?.text ? <pre className="max-h-[50dvh] overflow-auto rounded-xl bg-muted p-3 font-mono text-meta whitespace-pre-wrap">{progress.text}</pre> : null}
      {sessions.map((one) => <NativeSessionActivity key={`${one.target_node}:${one.work_id}`} session={one} compact />)}
      {stop ? <div className="grid gap-2">
        <Button type="button" variant="destructive" className="w-fit" disabled={!workspace.writable || action.busy !== null} onClick={() => void action.run("stop", stop, "已请求停止")}><Square aria-hidden="true" />停止任务</Button>
        <InlineError error={action.error} />
      </div> : null}
      <Details>
        {task ? <DetailRow label="任务编号" value={task.task_ref} mono /> : null}
        {session ? <DetailRow label="会话" value={`${session.thread_id} / ${session.turn_id ?? "-"}`} mono /> : null}
        {device ? <DetailRow label="设备编号" value={device} mono /> : null}
        {session?.detail || task?.detail ? <DetailRow label="说明" value={session?.detail || task?.detail || ""} /> : null}
      </Details>
    </div>}
  </aside>
}
