import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Link, useNavigate } from '@tanstack/react-router'
import { BrainCircuit, ChevronLeft, CircleQuestionMark, ListChecks, MessagesSquare, Reply, Settings, X } from 'lucide-react'
import { Details, EmptyState, InlineError, StatusPill, useAction } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Textarea } from '@/components/ui/textarea'
import { Input } from '@/components/ui/input'
import { Sheet, SheetContent, SheetHeader, SheetDescription, SheetTitle } from '@/components/ui/sheet'
import { Chat, type ChatEntry } from '@/features/chat/elements/chat'
import { PromptInput } from '@/features/chat/elements/prompt-input'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { networkKey, storageOf } from '@vibeface/client/workspace/storage.ts'
import type { ContextProjection, InputControlAction, InputIntent, InputReceipt, InputTarget, PendingMessage, QueuedInput, ScopeContext, WorkspaceHistory, WorkspaceTask } from '@vibeface/client/workspace/types.ts'
import { webPlatform } from '@/lib/platform'
import { ChannelForm } from './channels'
import { WorkspaceTaskLink, WorkspaceTaskPill } from './workspace-tasks'
import { QuestionCard } from './question-card'
import '@/features/chat/chat.css'

const deliveryLabels: Record<PendingMessage['delivery'], string> = {
  saved: '本地已保存', uploading: '本地已保存，附件准备中', submitting: '正在发送给连接的设备',
  accepted: '连接的设备已接收', unknown: '是否送达尚未确认',
}
const receiptLabels: Record<InputReceipt['stage'], string> = {
  entry_accepted: '连接的设备已接收', owner_accepted: '执行设备已接纳', submitting: '提交已登记，等待执行服务确认',
  provider_accepted: '执行服务已接受输入', associated: '已确认回合归属', refused: '已拒绝', unknown: '补充是否进入当前工作尚未确认',
}

function sameTarget(left: InputTarget, right: InputTarget) {
  if ((left.kind === 'task' || left.kind === 'channel_task') && (right.kind === 'task' || right.kind === 'channel_task')) return left.task_id === right.task_id
  return JSON.stringify(left) === JSON.stringify(right)
}

export function InputTargetLink({ target, conversationId }: { target: InputTarget; conversationId: string }) {
  const workspace = useWorkspace()
  const bot = workspace.bots.find((item) => item.conversation_id === conversationId)
  if (target.kind === 'task' || target.kind === 'channel_task') return <Link to="/tasks/$taskId" params={{ taskId: target.task_id }} className="inline-flex min-h-11 items-center text-meta underline underline-offset-4">打开所属任务</Link>
  if (target.kind === 'channel_member') return <Link to="/channels/$channelId" params={{ channelId: target.channel_id }} className="inline-flex min-h-11 items-center text-meta underline underline-offset-4">打开所属频道</Link>
  if (target.kind === 'work' && bot) return <Link to="/b/$botId/work/$workId" params={{ botId: bot.bot_id, workId: target.work_id }} search={{ from: 'inbox' }} className="inline-flex min-h-11 items-center text-meta underline underline-offset-4">打开所属工作</Link>
  return bot ? <Link to="/b/$botId" params={{ botId: bot.bot_id }} search={target.kind === 'routine' ? { routine: target.trigger } : {}} className="inline-flex min-h-11 items-center text-meta underline underline-offset-4">打开 {bot.name}{target.kind === 'routine' ? ' 的例行任务' : ''}</Link> : null
}

export function TargetContextButton({ target, conversationId }: { target: InputTarget; conversationId?: string }) {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const [context, setContext] = useState<ScopeContext | null>(null)
  const action = useAction()
  const title = target.kind === 'routine' ? '例行任务上下文' : target.kind === 'work' ? '工作上下文' : '输入目标上下文'
  return <><Button type="button" size="sm" variant="ghost" onClick={() => { setOpen(true); setContext(null); void action.run('scope-context', async () => setContext(await workspace.readScopeContext({ target, conversation_id: conversationId }))) }}><BrainCircuit aria-hidden="true" />查看目标上下文</Button><Sheet open={open} onOpenChange={setOpen}><SheetContent className="overflow-y-auto"><SheetHeader><SheetTitle>{title}</SheetTitle><SheetDescription>查看这个输入目标的来源、省略内容和补充归属。</SheetDescription></SheetHeader><div className="grid gap-3 px-4 pb-6"><InlineError error={action.error} />{context?.available ? <ScopeContextDetails context={context.context} /> : <p className="text-meta text-muted-foreground">{context ? context.reason || '这个目标尚无已编译的上下文。' : action.error ? '本次未读到目标上下文。' : '正在查询目标上下文。'}</p>}</div></SheetContent></Sheet></>
}

export function InputQueue({ target, channelId }: { target: InputTarget; channelId?: string }) {
  const workspace = useWorkspace()
  const action = useAction()
  const [editing, setEditing] = useState<string | null>(null)
  const [text, setText] = useState('')
  const [attachments, setAttachments] = useState<NonNullable<PendingMessage['attachments']>>([])
  const queued = (workspace.workspaceState?.inputs ?? []).filter((item) => item.deferred && !item.cancelled && (channelId ? (item.input.target.kind === 'channel_task' || item.input.target.kind === 'channel_member') && item.input.target.channel_id === channelId : sameTarget(item.input.target, target))).sort((left, right) => left.effective_order - right.effective_order)
  const control = (item: QueuedInput, command: InputControlAction) => action.run(item.input.message_id, async () => {
    const receipt = await workspace.queryInput({ message_id: item.input.message_id, target: item.input.target, conversation_id: item.conversation_id })
    if (!['entry_accepted', 'owner_accepted', 'refused'].includes(receipt.stage)) throw new Error('这条输入可能已提交，只能查询当前结果。')
    await workspace.controlInput({ message_id: item.input.message_id, expected_event: item.control_event, action: command })
    await workspace.readWorkspaceState()
    if (command.kind === 'edit') setEditing(null)
  })
  if (!queued.length) return null
  return <Details label={`稍后处理 ${queued.length} 条`} className="basis-full">
    <ul className="grid gap-3" aria-label="稍后输入队列">{queued.map((item) => {
      const lane = queued.filter((other) => sameTarget(other.input.target, item.input.target))
      const index = lane.findIndex((other) => other.input.message_id === item.input.message_id)
      const pending = workspace.pendingMessages.find((value) => value.message_id === item.input.message_id)
      const locked = Boolean(action.busy) || !workspace.writable || pending?.receipt && ['submitting', 'provider_accepted', 'associated', 'unknown'].includes(pending.receipt.stage)
      return <li key={item.input.message_id} className="grid gap-2 rounded-xl border bg-card p-3">
        {editing === item.input.message_id ? <div className="grid gap-2"><label className="grid gap-2">编辑稍后输入<Textarea value={text} maxLength={100000} onChange={(event) => setText(event.currentTarget.value)} /></label>{(item.input.attachments ?? []).map((attachment) => <label key={attachment.hash} className="flex min-h-11 items-center gap-2"><input type="checkbox" checked={attachments.some((value) => value.hash === attachment.hash)} onChange={(event) => { const checked = event.currentTarget.checked; setAttachments((current) => checked ? [...current, attachment] : current.filter((value) => value.hash !== attachment.hash)) }} />保留附件 {attachment.name}</label>)}<div className="flex flex-wrap gap-2"><Button type="button" size="sm" disabled={Boolean(locked)} onClick={() => void control(item, { kind: 'edit', text, attachments })}>保存输入修改</Button><Button type="button" size="sm" variant="ghost" onClick={() => setEditing(null)}>放弃编辑</Button></div></div> : <><p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{item.input.text || `${item.input.attachments?.length ?? 0} 个附件`}</p><div className="flex flex-wrap gap-2"><Button type="button" size="sm" variant="outline" disabled={Boolean(locked)} onClick={() => void control(item, { kind: 'release' })}>现在处理</Button><Button type="button" size="sm" variant="ghost" disabled={Boolean(locked)} onClick={() => { setEditing(item.input.message_id); setText(item.input.text); setAttachments(item.input.attachments ?? []) }}>编辑输入</Button><Button type="button" size="sm" variant="ghost" disabled={Boolean(locked)} onClick={() => void control(item, { kind: 'cancel' })}>取消稍后输入</Button><Button type="button" size="sm" variant="ghost" disabled={Boolean(locked) || index === 0} onClick={() => void control(item, { kind: 'move', before_message_id: lane[index - 1]?.input.message_id })}>上移</Button><Button type="button" size="sm" variant="ghost" disabled={Boolean(locked) || index === lane.length - 1} onClick={() => void control(item, { kind: 'move', ...(lane[index + 2] ? { before_message_id: lane[index + 2].input.message_id } : {}) })}>下移</Button></div></>}
        {locked ? <p className="text-muted-foreground">输入状态变化或正在提交时，先查询结果再管理。</p> : null}
      </li>
    })}</ul><InlineError error={action.error} /><p className="text-muted-foreground">操作由执行设备确认。已开始处理或结果未知的输入，只能查询状态。</p>
  </Details>
}

export function WorkspaceTaskDetail({ taskId }: { taskId: string }) {
  const workspace = useWorkspace()
  const action = useAction()
  const [contextOpen, setContextOpen] = useState(false)
  const [revising, setRevising] = useState(false)
  const [correction, setCorrection] = useState('')
  const task = workspace.workspaceState?.tasks.find((item) => item.task_id === taskId)
  const saved = workspace.pendingMessages.find((item) => (item.target.kind === 'task' || item.target.kind === 'channel_task') && item.target.task_id === taskId)
  const bot = workspace.bots.find((item) => item.bot_id === task?.owner_bot || !task && item.conversation_id === saved?.conversation_id)
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => { void latest.current.readWorkspaceState({ task_id: taskId }).catch(() => {}) }, [taskId, workspace.generation])
  if (!task && saved && bot) return <div className="flex min-h-0 min-w-0 flex-1 flex-col"><header className="grid gap-2 border-b px-4 py-3"><h2 className="text-heading font-semibold">任务已在本地保存</h2><p className="text-meta text-muted-foreground">正在读取任务记录。后续补充保留在这个任务。</p></header><ScopedConversation key={taskId} target={saved.target} conversationId={saved.conversation_id} expectedRevision={saved.expected_revision} label={`${bot.name} · 已保存的任务`} /></div>
  if (!task || !bot) return <EmptyState icon={ListChecks} title={workspace.workspaceState ? '尚未读到这个任务或所属 Bot' : '正在读取任务'} action={<Button type="button" variant="outline" onClick={() => void action.run('read', () => workspace.readWorkspaceState({ task_id: taskId }))}>重新读取</Button>} />
  const target: InputTarget = task.channel_id ? { kind: 'channel_task', channel_id: task.channel_id, task_id: task.task_id, bot_id: task.owner_bot } : { kind: 'task', task_id: task.task_id }
  const control = (choice: 'continue' | 'stop' | 'cancel' | 'defer') => action.run(choice, () => workspace.workspaceCommand({ kind: 'control_task', task_id: task.task_id, expected_revision: task.revision, action: choice }))
  return <div className="flex min-h-0 min-w-0 flex-1 flex-col">
    <header className="grid shrink-0 gap-3 border-b px-4 py-3 sm:px-6">
      <div className="flex min-w-0 flex-wrap items-start gap-2"><Button asChild type="button" variant="ghost" size="icon" className="-ms-2 shrink-0"><Link to="/tasks" aria-label="返回任务"><ChevronLeft aria-hidden="true" /></Link></Button><div className="min-w-0 flex-1"><h2 className="[overflow-wrap:anywhere] text-heading font-semibold">{task.title}</h2><p className="mt-1 text-meta text-muted-foreground">{bot.name} · 第 {task.revision} 版</p></div><WorkspaceTaskPill task={task} /></div>
      {task.waiting_reason ? <p role="status" className="text-meta text-warning">{task.waiting_reason}</p> : null}
      <div className="flex flex-wrap gap-2"><Button type="button" size="sm" variant="outline" onClick={() => setContextOpen((value) => !value)}><BrainCircuit aria-hidden="true" />{contextOpen ? '收起上下文' : '上下文和历史'}</Button><Button type="button" size="sm" variant="outline" onClick={() => { setCorrection(task.request); setRevising((value) => !value) }}>纠正任务</Button>{task.channel_id ? <Button asChild size="sm" variant="ghost"><Link to="/channels/$channelId" params={{ channelId: task.channel_id }}>打开频道</Link></Button> : null}<Button type="button" size="sm" variant="ghost" disabled={!workspace.writable || Boolean(action.busy)} onClick={() => void control('defer')}>稍后处理</Button><Button type="button" size="sm" variant="ghost" disabled={!workspace.writable || Boolean(action.busy)} onClick={() => void control('continue')}>继续任务</Button><Button type="button" size="sm" variant="ghost" disabled={!workspace.writable || Boolean(action.busy)} onClick={() => void control('cancel')}>取消任务</Button></div>
      <InlineError error={action.error} />
      {revising ? <form className="grid gap-2" onSubmit={(event) => { event.preventDefault(); void action.run('revise', async () => {
        const result = await workspace.workspaceCommand({ kind: 'revise_task', task_id: task.task_id, expected_revision: task.revision, request: correction.trim(), expected_result: task.expected_result, dependencies: task.dependencies })
        const revised = result.tasks.find((item) => item.task_id === task.task_id)
        if (!revised) throw new Error('任务修订已提交，尚未确认新版本。请查询任务。')
        await workspace.sendMessage(correction.trim(), [], undefined, { target, expected_revision: revised.revision, conversation_id: bot.conversation_id })
        setRevising(false)
      }) }}><label className="grid gap-2 text-meta">更新任务交代<Textarea required value={correction} maxLength={100000} onChange={(event) => setCorrection(event.currentTarget.value)} /></label><p className="text-meta text-muted-foreground">保留任务身份并新增版本。旧版本结果不能完成这次更新。</p><Button type="submit" className="justify-self-start" disabled={!workspace.writable || Boolean(action.busy)}>保存修订并补充</Button></form> : null}
      {contextOpen ? <WorkspaceTaskContext task={task} /> : null}
    </header>
    <ScopedConversation key={task.task_id} target={target} conversationId={bot.conversation_id} task={task} label={`${bot.name} · ${task.title}`} onStop={() => control('stop')} />
  </div>
}

export function ChannelWorkspace({ channelId }: { channelId: string }) {
  const workspace = useWorkspace()
  const { ready } = useDevice()
  const storage = storageOf(webPlatform.storage)
  const selectionKey = networkKey(`channel-target:${channelId}`, ready?.principal.network_id ?? '')
  const cached = storage.readStorage<unknown>(selectionKey, null)
  const cachedBot = cached && typeof cached === 'object' && 'bot_id' in cached && typeof cached.bot_id === 'string' ? cached.bot_id : ''
  const cachedTask = cached && typeof cached === 'object' && 'task_id' in cached && typeof cached.task_id === 'string' ? cached.task_id : null
  const selectionRevision = useRef(0)
  const savedSelection = selectionRevision.current
  const [settings, setSettings] = useState(false)
  const [recipient, setRecipient] = useState(cachedBot)
  const [replyTask, setReplyTask] = useState<string | null>(cachedTask)
  const [savedInput, setSavedInput] = useState<PendingMessage | null>(null)
  const [readError, setReadError] = useState<unknown>(null)
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => { void latest.current.readWorkspaceState({ channel_id: channelId }).then(() => replyTask ? latest.current.readWorkspaceState({ task_id: replyTask }) : undefined).catch(setReadError) }, [channelId, workspace.generation, replyTask])
  const channel = workspace.workspaceState?.channels.find((item) => item.channel_id === channelId)
  const botId = recipient || channel?.coordinator_bot
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const tasks = (workspace.workspaceState?.tasks ?? []).filter((item) => item.channel_id === channelId)
  const selectedTask = replyTask ? tasks.find((item) => item.task_id === replyTask) : [...tasks].reverse().find((item) => item.owner_bot === botId && !['completed', 'cancelled'].includes(item.state))
  const pending = savedInput ?? [...workspace.pendingMessages].reverse().find((item) => item.target.kind === 'channel_task' && item.target.channel_id === channelId && item.target.task_id === replyTask)
  if (!channel || !bot) return <div className="grid gap-3"><InlineError error={readError} /><EmptyState icon={MessagesSquare} title={workspace.workspaceState ? '尚未读到这个频道或协调 Bot' : '正在读取频道'} action={<Button type="button" variant="outline" onClick={() => void workspace.readWorkspaceState({ channel_id: channelId }).catch(setReadError)}>重新读取</Button>} /></div>
  const target: InputTarget = selectedTask ? { kind: 'channel_task', channel_id: channelId, task_id: selectedTask.task_id, bot_id: selectedTask.owner_bot } : replyTask ? { kind: 'channel_task', channel_id: channelId, task_id: replyTask, bot_id: bot.bot_id } : { kind: 'channel_member', channel_id: channelId, bot_id: bot.bot_id }
  return <div className="flex min-h-0 min-w-0 flex-1">
    <div className="flex min-h-0 min-w-0 flex-1 flex-col"><header className="grid shrink-0 gap-2 border-b px-4 py-3 sm:px-6"><div className="flex min-w-0 items-start gap-3"><div className="min-w-0 flex-1"><h2 className="[overflow-wrap:anywhere] text-heading font-semibold">{channel.title}</h2>{channel.purpose ? <p className="mt-1 text-meta text-muted-foreground">{channel.purpose}</p> : null}</div><Button type="button" variant="ghost" size="icon" aria-label="频道设置" onClick={() => setSettings(true)}><Settings aria-hidden="true" /></Button></div><p className="text-meta text-muted-foreground">{channel.members.map((id) => workspace.bots.find((item) => item.bot_id === id)?.name ?? id).join('、')} · 默认协调 {workspace.bots.find((item) => item.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</p>
      <Details label={`频道任务 · ${tasks.length}`} className="lg:hidden"><div className="grid gap-2">{tasks.map((task) => <WorkspaceTaskLink key={task.task_id} task={task} />)}</div></Details></header>
      <ScopedConversation key={channelId} target={target} conversationId={(workspace.bots.find((item) => item.bot_id === selectedTask?.owner_bot) ?? bot).conversation_id} task={selectedTask} expectedRevision={selectedTask?.revision ?? pending?.expected_revision} channelId={channelId} onSaved={(input) => { if (savedSelection !== selectionRevision.current || !workspace.isCurrent()) return; if (input.target.kind !== "channel_task" || input.target.channel_id !== channelId) return; setSavedInput(input); setRecipient(input.target.bot_id); setReplyTask(input.target.task_id); storage.writeStorage(selectionKey, { bot_id: input.target.bot_id, task_id: input.target.task_id }) }} label={`${workspace.bots.find((item) => item.bot_id === selectedTask?.owner_bot)?.name ?? bot.name}${selectedTask ? ` · ${selectedTask.title}` : replyTask ? ' · 已保存的频道任务' : ' · 新频道任务'}`} onReply={(taskId, recipientBot) => { selectionRevision.current++; setSavedInput(null); setReplyTask(taskId); setRecipient(recipientBot); storage.writeStorage(selectionKey, { bot_id: recipientBot, task_id: taskId }) }} targetControl={<><label className="flex min-w-0 items-center gap-2"><span className="shrink-0">交给</span><NativeSelect size="sm" aria-label="频道消息目标" value={botId} onChange={(event) => { selectionRevision.current++; setRecipient(event.currentTarget.value); setReplyTask(null); setSavedInput(null); storage.writeStorage(selectionKey, { bot_id: event.currentTarget.value }) }}>{channel.members.map((id) => <NativeSelectOption key={id} value={id}>{workspace.bots.find((item) => item.bot_id === id)?.name ?? id}{id === channel.coordinator_bot ? ' · 默认协调' : ''}</NativeSelectOption>)}</NativeSelect></label>{replyTask ? <Button type="button" variant="ghost" size="sm" onClick={() => { selectionRevision.current++; setReplyTask(null); setSavedInput(null); storage.writeStorage(selectionKey, { bot_id: recipient }) }}><X aria-hidden="true" />结束回复选择</Button> : null}</>} />
    </div>
    <aside className="hidden w-[22rem] shrink-0 overflow-y-auto border-s p-4 lg:block" aria-label="频道任务"><h3 className="mb-3 font-medium">频道任务</h3><div className="grid gap-3">{tasks.map((task) => <WorkspaceTaskLink key={task.task_id} task={task} />)}{!tasks.length ? <p className="text-meta text-muted-foreground">发送第一条交代，创建频道任务。</p> : null}</div></aside>
    {settings ? <ChannelForm key={channel.event_hash} open={settings} onOpenChange={setSettings} channel={channel} /> : null}
  </div>
}

function ScopedConversation({ target, conversationId, label, task, channelId, expectedRevision, targetControl, onReply, onStop, onSaved }: { target: InputTarget; conversationId: string; label: string; task?: WorkspaceTask; channelId?: string; expectedRevision?: number; onSaved?: (input: PendingMessage) => void; targetControl?: ReactNode; onReply?: (taskId: string, botId: string) => void; onStop?: () => Promise<unknown> }) {
  const workspace = useWorkspace()
  const { ready } = useDevice()
  const navigate = useNavigate()
  const routeActive = useRef(true)
  useEffect(() => { routeActive.current = true; return () => { routeActive.current = false } }, [])
  const [history, setHistory] = useState<WorkspaceHistory | null>(null)
  const [query, setQuery] = useState('')
  const [error, setError] = useState<unknown>(null)
  const [intent, setIntent] = useState<InputIntent>('continue')
  const [files, setFiles] = useState<File[]>([])
  const scopeTaskId = task?.task_id ?? (target.kind === 'task' || target.kind === 'channel_task' ? target.task_id : undefined)
  const storage = storageOf(webPlatform.storage)
  const draftKey = networkKey(`draft:${channelId ? `channel:${channelId}` : `task:${scopeTaskId}`}`, ready?.principal.network_id ?? '')
  const [text, setText] = useState(() => { const saved = storage.readStorage<unknown>(draftKey, ''); return typeof saved === 'string' ? saved : '' })
  const latest = useRef(workspace)
  latest.current = workspace
  const filter = channelId ? { channel_id: channelId } : { task_id: scopeTaskId }
  const refresh = async () => {
    const current = latest.current
    if (!channelId && !scopeTaskId) throw new Error('输入范围尚未确认。')
    const result = await current.readWorkspaceHistory(filter)
    if (current.isCurrent()) { setHistory(result); setError(null) }
  }
  useEffect(() => {
    let active = true
    const read = () => { if (active) void refresh().catch((cause) => { if (active) setError(cause) }) }
    read()
    const timer = setInterval(read, 3000)
    return () => { active = false; clearInterval(timer) }
  }, [workspace.generation, channelId, scopeTaskId])
  const setDraft = (value: string) => { setText(value); storage.writeStorage(draftKey, value) }
  const pending = workspace.pendingMessages.filter((item) => channelId ? (item.target.kind === 'channel_task' || item.target.kind === 'channel_member') && item.target.channel_id === channelId : sameTarget(item.target, target))
  const entries: ChatEntry[] = []
  for (const event of (history?.events ?? []).filter((item) => !query.trim() || JSON.stringify(item.body).toLocaleLowerCase().includes(query.trim().toLocaleLowerCase()))) {
    const body = event.body
    const actor = event.speaker
    const speaker = actor.kind === 'bot' ? workspace.bots.find((bot) => bot.bot_id === actor.bot_id)?.name ?? 'Bot 未加载' : actor.kind === 'human' ? '你' : workspace.nodes.find((node) => node.device_id === actor.device_id)?.name ?? '执行设备'
    if (body.type === 'input_posted' || body.type === 'message_posted') {
      entries.push({ key: event.event_hash, anchor: body.type === 'input_posted', message: { message_id: body.message_id, event_hash: event.event_hash, role: body.type === 'input_posted' ? 'user' : channelId && actor.kind === 'bot' ? 'bot' : body.role, text: body.text, created_at_ms: event.created_at_ms, attachments: body.attachments, ...(body.type === 'message_posted' ? { in_reply_to: body.in_reply_to, status: body.status, ...(channelId && actor.kind === 'bot' ? { sender: { conversation_id: workspace.bots.find((bot) => bot.bot_id === actor.bot_id)?.conversation_id ?? '', name: speaker } } : {}) } : {}) } })
      if (body.type === 'input_posted' && body.target.kind === 'channel_task' && onReply) { const reply = body.target; entries.push({ key: `reply:${event.event_hash}`, node: <Button type="button" variant="ghost" size="sm" onClick={() => onReply(reply.task_id, reply.bot_id)}><Reply aria-hidden="true" />回复这个任务</Button> }) }
    } else if (body.type === 'work_finished') entries.push({ key: event.event_hash, node: <div className="grid gap-2 rounded-xl border bg-card p-4"><p className="text-meta font-medium">工作结果 · {body.outcome === 'completed' ? '已完成' : body.outcome === 'interrupted' ? '已停止' : '需要留意'}</p><p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{body.result_text || body.failure_reason || '没有结果文本'}</p></div> })
    else if (body.type === 'workspace_commanded') {
      const command = body.command
      if (command.kind === 'assignment_progress' && command.result) entries.push({ key: event.event_hash, node: <div className="grid gap-2 rounded-xl border bg-card p-4"><p className="text-meta text-muted-foreground">第 {command.revision} 版提交的结果，执行状态以任务记录为准</p><p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{command.result}</p></div> })
      else if (command.kind === 'post_channel_message') entries.push({ key: event.event_hash, node: <div className="grid gap-2 rounded-xl border bg-card p-4"><p className="text-meta text-muted-foreground">{speaker} · 频道记录</p><p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{command.text}</p>{command.attachments.length ? <p className="text-meta text-muted-foreground">{command.attachments.length} 个附件</p> : null}</div> })
      else if (command.kind === 'create_task') { const item = workspace.workspaceState?.tasks.find((task) => task.task_id === command.task_id); if (item) entries.push({ key: event.event_hash, node: <WorkspaceTaskLink task={item} /> }) }
      else if (command.kind === 'revise_task') entries.push({ key: event.event_hash, node: <p className="text-meta text-muted-foreground">任务交代已更新为第 {command.expected_revision + 1} 版。</p> })
    }
  }
  const postedIds = new Set((history?.events ?? []).flatMap((event) => event.body.type === 'input_posted' ? [event.body.message_id] : []))
  for (const question of workspace.questions.filter((question) => question.target && (channelId ? (question.target.kind === 'channel_task' || question.target.kind === 'channel_member') && question.target.channel_id === channelId : sameTarget(question.target, target)))) entries.push({ key: `question:${question.question_id}`, node: <QuestionCard question={question} inTask /> })
  for (const item of pending) if (!postedIds.has(item.message_id)) entries.push({ key: item.message_id, anchor: true, message: { message_id: item.message_id, event_hash: item.message_id, role: 'user', text: item.text, created_at_ms: item.created_at_ms, state: 'sending' } })
  for (const item of pending) entries.push({ key: `delivery:${item.message_id}`, node: <WorkspaceInputStatus pending={item} refresh={refresh} /> })
  const busy = task && ['running', 'update_pending'].includes(task.state)
  const sendLabel = intent === 'defer' ? '保存为稍后处理' : intent === 'new_task' ? '发送为新任务' : busy ? '补充当前工作' : '发送'
  return <section className="conversation-screen" aria-label={channelId ? '频道对话' : '任务对话'}>
    <label className="mx-auto grid w-full max-w-(--page-width) shrink-0 gap-1 px-4 pt-3 text-meta text-muted-foreground sm:px-6">{channelId ? '搜索当前频道已加载的记录' : '搜索当前任务已加载的记录'}<Input type="search" value={query} onChange={(event) => setQuery(event.currentTarget.value)} placeholder="输入关键词" /></label>
    <Chat key={channelId ?? scopeTaskId} entries={entries} empty={<EmptyState icon={MessagesSquare} title={history ? '发送一条交代开始对话' : '正在读取任务历史'} />} />
    <PromptInput value={text} onChange={setDraft} files={files} onFiles={setFiles} hint="" reason={workspace.readOnlyNotice || workspace.writeNotice} disabled={!workspace.canSaveMessages} busy={false} label={`发给 ${label}`} sendLabel={sendLabel} target={<><span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{busy ? '补充给' : '发送给'} · {label}</span>{targetControl}<NativeSelect size="sm" aria-label="发送方式" value={intent} onChange={(event) => { const value = event.currentTarget.value; if (value === 'continue' || value === 'new_task' || value === 'defer') setIntent(value) }}><NativeSelectOption value="continue">{busy ? '补充当前工作' : '发送'}</NativeSelectOption><NativeSelectOption value="new_task">新任务</NativeSelectOption><NativeSelectOption value="defer">稍后处理</NativeSelectOption></NativeSelect><InputQueue target={target} channelId={channelId} /></>} onStop={workspace.writable && task && !['completed', 'cancelled', 'deferred'].includes(task.state) ? () => { void (onStop ? onStop() : workspace.stopInputTarget({ target, expected_revision: task.revision })).catch(setError) } : undefined} onSubmit={() => {
      if (!text.trim() && !files.length) return
      const submitted = text
      const selectedFiles = files
      setError(null)
      void workspace.sendMessage(submitted, selectedFiles, undefined, { target, intent, conversation_id: conversationId, ...((expectedRevision ?? task?.revision) !== undefined ? { expected_revision: expectedRevision ?? task?.revision } : {}), onSaved: (input) => { if (!routeActive.current || !workspace.isCurrent()) return; onSaved?.(input); if (!channelId && (input.target.kind === 'task' || input.target.kind === 'channel_task') && input.target.task_id !== scopeTaskId) void navigate({ to: '/tasks/$taskId', params: { taskId: input.target.task_id } }) } }).then((saved) => {
        if (saved && latest.current.isCurrent()) { setText((current) => { if (current !== submitted) return current; storage.writeStorage(draftKey, ''); return '' }); setFiles((current) => current === selectedFiles ? [] : current); setIntent('continue'); void refresh().catch(setError) }
      }).catch(setError)
    }}><InlineError error={error} action={{ label: '重新读取', onClick: () => void refresh().catch(setError) }} /><p className="text-meta text-muted-foreground">{history ? `已加载最近 ${history.events.length} 条${history.coverage.truncated ? '，更早的记录未加载' : ''}。` : '尚未读取历史。'}执行服务是否接收、输入归入哪个回合，请查看每条输入的状态。</p></PromptInput>
  </section>
}

export function WorkspaceInputStatus({ pending, refresh }: { pending: PendingMessage; refresh?: () => Promise<void> }) {
  const workspace = useWorkspace()
  const action = useAction()
  const receipt = pending.receipt
  return <div role="status" className="grid gap-2 rounded-lg bg-muted/60 px-3 py-2 text-meta">
    <p className={pending.rejected || pending.delivery === 'unknown' || receipt?.stage === 'unknown' ? 'text-warning' : 'text-muted-foreground'}>{pending.rejected ? `已拒绝 · ${pending.rejected}` : receipt ? receiptLabels[receipt.stage] : deliveryLabels[pending.delivery]}</p>
    {receipt?.actual_turn_id ? <p className="break-all text-muted-foreground">实际回合 · {receipt.actual_turn_id}</p> : null}
    {receipt?.reason ? <p className="text-muted-foreground">{receipt.reason}</p> : null}
    <div className="flex flex-wrap gap-2"><Button type="button" variant="ghost" size="sm" disabled={Boolean(action.busy)} onClick={() => void action.run('query', async () => { await workspace.queryInput({ message_id: pending.message_id, target: pending.target, conversation_id: pending.conversation_id }); await refresh?.() })}><CircleQuestionMark aria-hidden="true" />查询结果</Button>{pending.rejected ? <Button type="button" variant="ghost" size="sm" disabled={Boolean(action.busy)} onClick={() => void action.run('release', () => workspace.releaseMessagePending(pending.message_id))}>不再发送</Button> : null}</div>
    <InlineError error={action.error} />
  </div>
}

function WorkspaceTaskContext({ task }: { task: WorkspaceTask }) {
  const workspace = useWorkspace()
  const [context, setContext] = useState<ScopeContext | null>(null)
  const [error, setError] = useState<unknown>(null)
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => {
    let active = true
    const target: InputTarget = task.channel_id ? { kind: 'channel_task', channel_id: task.channel_id, task_id: task.task_id, bot_id: task.owner_bot } : { kind: 'task', task_id: task.task_id }
    setContext(null); setError(null)
    void latest.current.readScopeContext({ target }).then((value) => { if (active) { setContext(value); setError(null) } }, (cause) => { if (active) setError(cause) })
    return () => { active = false }
  }, [task.task_id, task.revision, workspace.generation])
  const assignment = workspace.workspaceState?.assignments.find((item) => item.assignment_id === task.current_assignment)
  return <div className="grid max-h-[40dvh] gap-3 overflow-y-auto rounded-xl bg-muted/50 p-3 text-meta" aria-label="任务上下文和来源">
    <div className="flex flex-wrap items-center gap-2"><BrainCircuit aria-hidden="true" className="size-4" /><h3 className="font-medium">本轮来源</h3><StatusPill>任务独立对话</StatusPill></div>
    <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">原始交代 · {task.request}</p><p>期望结果 · {task.expected_result}</p>{assignment?.work_id ? <Button asChild variant="outline" size="sm" className="justify-self-start"><Link to="/b/$botId/work/$workId" params={{ botId: task.owner_bot, workId: assignment.work_id }}>查看执行工作</Link></Button> : null}
    <p>任务第 {task.revision} 版{assignment ? ` · 当前执行从第 ${assignment.started_revision} 版开始` : ' · 尚未读到执行尝试'}</p>
    <p>依赖任务 · {task.dependencies.length ? task.dependencies.map((id) => workspace.workspaceState?.tasks.find((item) => item.task_id === id)?.title ?? id).join('、') : '无'}</p>
    <Details label="来源与固定依据"><p className="break-all">任务来源事件 · {task.request_event}</p>{task.source_events.map((source) => <p key={source} className="break-all">引用来源 · {source}</p>)}{assignment ? <p className="break-all">本轮依据 · {JSON.stringify(assignment.basis)}</p> : null}</Details>
    <p className="text-muted-foreground">任务历史和 Bot 的长期记忆分别保存。执行服务的会话缓存由执行设备管理。本轮采用的内容以来源记录为准，补充的接收与回合归属以输入状态为准。</p>
    <Button type="button" variant="ghost" size="sm" className="justify-self-start" onClick={() => { setError(null); void workspace.readScopeContext({ target: { kind: "task", task_id: task.task_id } }).then(setContext, setError) }}>重新读取上下文</Button>
    <InlineError error={error} />
    {context?.available ? <ScopeContextDetails context={context.context} /> : <p className="text-muted-foreground">{context ? context.reason || '这个任务尚无已编译的上下文。' : error ? '本次未读到任务上下文。' : '正在查询这个任务的上下文。'}</p>}
    <Button asChild variant="ghost" size="sm" className="justify-self-start"><Link to="/b/$botId" params={{ botId: task.owner_bot }}>打开 Bot 上下文</Link></Button>
  </div>
}

export function ScopeContextDetails({ context }: { context: ContextProjection }) {
  const kinds: Record<ContextProjection['sources'][number]['kind'], string> = { trigger: '本轮交代', message: '共享消息', work: '工作结果', dispatch: '工作指派', checkpoint: '历史摘要', handoff: '任务交接', memory: '持久记忆', question: '已知问题', attachment: '附件' }
  return <div role="region" className="grid gap-3 text-meta" aria-label="实际上下文来源与补充回执">
    <h3 className="font-medium">本轮上下文引用</h3>
    <p className="text-muted-foreground">{context.stage === 'compiled' ? '上下文已整理，尚未确认执行服务收到。' : '以下来源来自这个执行范围的上下文编译记录。'}</p>
    <ul className="flex flex-wrap gap-x-4 gap-y-1">{Object.entries(kinds).map(([kind, name]) => { const count = context.sources.filter((source) => source.kind === kind).length; return count ? <li key={kind}>{name} {count} 项</li> : null })}</ul>
    <p>因预算省略 · 消息 {context.coverage.messages_omitted} 条 · 附件 {context.coverage.attachments_omitted} 个 · 记忆 {context.coverage.memory_omitted} 条</p>
    {context.sources_omitted ? <p className="text-muted-foreground">另有 {context.sources_omitted} 个来源引用未在这份有界记录中展开。</p> : null}
    <Details label="查看来源引用"><ul className="grid gap-2">{context.sources.map((source, index) => <li key={`${source.kind}:${index}`} className="break-all">{kinds[source.kind]} · {source.reference ?? source.event_id ?? (source.author ? `${source.author}:${source.seq ?? ''}` : '未提供引用编号')}</li>)}</ul></Details>
    <div className="grid gap-2"><h3 className="font-medium">补充归属</h3>{context.inputs.length ? context.inputs.map((input, index) => <div key={input.message_id} className="grid gap-1 rounded-lg bg-background/70 p-2"><p>第 {index + 1} 条补充 · {receiptLabels[input.stage]}</p><Details label="消息和回合编号"><p className="break-all">消息 · {input.message_id}</p><p className="break-all">目标回合 · {input.expected_turn_id}</p><p className="break-all">实际回合 · {input.actual_turn_id ?? '尚未确认'}</p></Details></div>) : <p className="text-muted-foreground">这个范围的上下文记录没有补充回执。</p>}{context.inputs_omitted ? <p className="text-muted-foreground">另有 {context.inputs_omitted} 条补充回执未展开。</p> : null}</div>
  </div>
}
