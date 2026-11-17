import { InlineError,StatusPill,useAction } from '@/components/arc/product-kit'
import taskStyles from "@/components/arc/task-compositions.module.css"
import { Link,useNavigate } from '@tanstack/react-router'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { newId } from '@vibeface/client/workspace/storage.ts'
import type { WorkspaceCommand,WorkspaceTask } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Drawer,DrawerContent,DrawerTrigger } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Input } from '@vibeface/uiarc/input/input.tsx'
import { Select } from "@vibeface/uiarc/select/select.tsx"
import { Textarea } from '@vibeface/uiarc/textarea/textarea.tsx'
import { ChevronRight,Plus } from 'lucide-react'
import { useEffect,useRef,useState } from 'react'

const workspaceTaskLabels: Record<WorkspaceTask['state'], string> = {
  not_started: '未开始', running: '执行中', waiting_dependencies: '等待依赖', waiting_user: '等待你决定',
  update_pending: '更新待承接', stop_pending: '停止待确认', needs_reconciliation: '结果待确认',
  completed: '已完成', cancelled: '已取消', deferred: '稍后处理',
}

export function WorkspaceTaskPill({ task }: { task: WorkspaceTask }) {
  return <StatusPill tone={task.state === 'completed' ? 'success' : task.state === 'running' ? 'info' : ['needs_reconciliation', 'waiting_user', 'stop_pending'].includes(task.state) ? 'warning' : 'neutral'}>{workspaceTaskLabels[task.state]}</StatusPill>
}

export function WorkspaceTasks({ botFilter = 'all', filter = 'all', onReadError, showEmpty = true }: { botFilter?: string; filter?: 'all' | 'attention' | 'working' | 'waiting'; onReadError?: (cause: unknown) => void; showEmpty?: boolean }) {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => {
    let current = true
    setError(null)
    void latest.current.readWorkspaceState().catch((cause) => { if (current) { setError(cause); onReadError?.(cause) } })
    return () => { current = false }
  }, [workspace.generation])
  useEffect(() => { setError(null) }, [workspace.workspaceState])
  const groups = { attention: ['waiting_user', 'needs_reconciliation', 'update_pending', 'stop_pending'], working: ['running'], waiting: ['not_started', 'waiting_dependencies', 'deferred'] }
  const tasks = (workspace.workspaceState?.tasks ?? []).filter((task) => (botFilter === 'all' || task.owner_bot === botFilter) && (filter === 'all' || groups[filter].includes(task.state)))
  return <Drawer open={open} onOpenChange={setOpen}><section className="grid gap-3" aria-labelledby="workspace-tasks-heading">
    <div className="flex items-center justify-between gap-3"><h2 id="workspace-tasks-heading" className="text-lg font-medium">任务对话</h2><DrawerTrigger asChild><Button type="button" variant="secondary" disabled={!workspace.writable || !workspace.bots.length}><Plus aria-hidden="true" />新任务</Button></DrawerTrigger></div>
    <p className="text-sm text-secondary">每个任务保留独立对话。补充内容会继续发给这个任务。</p>
    <InlineError error={error} action={{ label: '重新读取', onClick: () => { setError(null); void workspace.readWorkspaceState().catch(setError) } }} />
    {tasks.length ? <ul className="grid gap-3">{tasks.map((task) => <li key={task.task_id}><WorkspaceTaskLink task={task} /></li>)}</ul> : showEmpty ? <p role="status" className="text-sm text-secondary">{workspace.workspaceState ? botFilter !== 'all' || filter !== 'all' ? '没有符合筛选的任务对话' : '还没有任务对话' : '正在读取任务对话'}</p> : null}
    <CreateWorkspaceTask onOpenChange={setOpen} botId={botFilter === 'all' ? undefined : botFilter} />
  </section></Drawer>
}

export function WorkspaceTaskLink({ task }: { task: WorkspaceTask }) {
  const workspace = useWorkspace()
  const bot = workspace.bots.find((item) => item.bot_id === task.owner_bot)
  return <Link to="/tasks/$taskId" params={{ taskId: task.task_id }} className={`${taskStyles.record} [overflow-wrap:anywhere]`}>
    <span className="flex min-w-0 flex-wrap items-start gap-2"><span className="min-w-0 flex-1 [overflow-wrap:anywhere] font-medium">{task.title}</span><WorkspaceTaskPill task={task} /><ChevronRight aria-hidden="true" className="size-4 shrink-0 text-secondary" /></span>
    <span className="text-sm text-secondary">{bot?.name ?? '所属 Bot 未加载'} · 第 {task.revision} 版{task.channel_id ? ' · 频道任务' : ''}</span>
    {task.waiting_reason ? <span className="text-sm text-warning">{task.waiting_reason}</span> : null}
  </Link>
}

function CreateWorkspaceTask({ onOpenChange, botId }: { onOpenChange: (value: boolean) => void; botId?: string }) {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const action = useAction()
  const [selected, setSelected] = useState(botId ?? workspace.selectedBot?.bot_id ?? workspace.bots[0]?.bot_id ?? '')
  const [title, setTitle] = useState('')
  const [request, setRequest] = useState('')
  const [invalid, setInvalid] = useState<'title' | 'request' | null>(null)
  const titleInput = useRef<HTMLInputElement>(null)
  const requestInput = useRef<HTMLTextAreaElement>(null)
  const taskId = useRef(newId())
  const submission = useRef<Extract<WorkspaceCommand, { kind: 'create_task' }> | null>(null)
  useEffect(() => { if (!submission.current) setSelected(botId ?? '') }, [botId])
  const selectedId = workspace.bots.find((bot) => bot.bot_id === selected)?.bot_id
    ?? workspace.bots.find((bot) => bot.bot_id === (botId ?? workspace.selectedBot?.bot_id))?.bot_id ?? workspace.bots[0]?.bot_id ?? ''
  const ownerBotId = submission.current?.owner_bot ?? selectedId
  return <DrawerContent title="新任务" description="选择 Bot，任务后续的补充留在同一个对话里。" className="overflow-y-auto">
    <form className="grid gap-4" noValidate onSubmit={(event) => {
      event.preventDefault()
      if (!submission.current && !title.trim()) { setInvalid('title'); titleInput.current?.focus(); return }
      if (!submission.current && !request.trim()) { setInvalid('request'); requestInput.current?.focus(); return }
      setInvalid(null)
      void action.run('create-task', async () => {
      const owner = workspace.bots.find((bot) => bot.bot_id === ownerBotId)
      if (!owner) throw new Error('请选择一个 Bot。')
      const command: Extract<WorkspaceCommand, { kind: 'create_task' }> = submission.current ?? { kind: 'create_task', task_id: taskId.current, owner_bot: owner.bot_id, title: title.trim(), request: request.trim(), expected_result: title.trim(), source_events: [], dependencies: [], reply_only: false }
      submission.current = command
      const result = await workspace.workspaceCommand(command)
      const task = result.tasks.find((item) => item.task_id === taskId.current)
      if (!task) throw new Error('任务已提交，尚未读到任务记录。重新查询后继续。')
      const saved = await workspace.sendMessage(command.request, [], undefined, { target: { kind: 'task', task_id: task.task_id }, expected_revision: task.revision, conversation_id: owner.conversation_id })
      if (!saved) throw new Error('任务已创建，首次交代尚未保存。请重试保存。')
      onOpenChange(false)
      await navigate({ to: '/tasks/$taskId', params: { taskId: task.task_id } })
      submission.current = null; taskId.current = newId(); setTitle(''); setRequest('')
    }) }}>
      <div className="grid gap-2 text-sm font-medium"><Select required label="交给哪个 Bot" placeholder="请选择一个 Bot" disabled={Boolean(submission.current)} value={ownerBotId ? "arc:" + ownerBotId : ""} onValueChange={selected => { const value = selected.slice(4);  setSelected(value)  }} options={workspace.bots.map((bot) => ({ value: "arc:" + bot.bot_id, label: bot.name }))} /></div>
      <div className="grid gap-2 text-sm font-medium"><Input ref={titleInput} label="任务名称" error={invalid === 'title' ? '还没填任务名称' : undefined} required readOnly={Boolean(submission.current)} maxLength={512} value={title} onChange={(event) => { setTitle(event.currentTarget.value); if (invalid === 'title') setInvalid(null) }} placeholder="例如：检查官网导航" /></div>
      <div className="grid gap-2 text-sm font-medium"><Textarea ref={requestInput} label="交代内容" error={invalid === 'request' ? '还没填交代内容' : undefined} required readOnly={Boolean(submission.current)} maxLength={100000} value={request} onChange={(event) => { setRequest(event.currentTarget.value); if (invalid === 'request') setInvalid(null) }} placeholder="描述要完成的事情和需要保留的约束" /></div>
      <InlineError error={action.error} />{submission.current ? <p role="status" className="text-sm text-secondary">原交代已保留，重试会继续确认同一个任务。</p> : null}
      <div className="flex flex-wrap justify-end gap-2">
        <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
        <Button type="submit" disabled={Boolean(action.busy) || !workspace.writable || !selectedId}>{action.busy ? '正在保存任务' : '创建并发送'}</Button>
      </div>
    </form>
  </DrawerContent>
}
