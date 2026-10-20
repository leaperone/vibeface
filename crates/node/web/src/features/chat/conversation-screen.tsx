import { useEffect, useRef, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ArrowRightLeft, Ban, Bot, ChevronRight, CircleCheck, CircleX, Laptop, ListChecks, LoaderCircle, MessageSquareWarning } from 'lucide-react'
import { ActionCard, InlineError } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { useDevice } from '@/features/identity/context'
import { useWorkspace } from '@/features/workspace/context'
import { humanError } from '@/lib/errors'
import { ActionButton } from '@/features/workspace/present'
import { ChatSkeleton } from '@/features/workspace/workspace-shell'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { AlertCard } from '@/features/workspace/alert-card'
import { nodeName, progressLabel, shownState, taskActive, taskLabel } from '@/features/workspace/labels'
import type { AssistantChangeRecord, ManagerJob, WorkRecord } from '@/features/workspace/types'
import { placeChanges, routineName, routineTrigger } from '@/features/workspace/assistant'
import { pendingChecks } from '@/features/workspace/model'
import { ChangeCard } from './change-card'
import { Chat, type ChatEntry } from './elements/chat'
import { Message, type ChatMessage } from './elements/message'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import './chat.css'

const examples = ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？']
const replyStates: Record<string, string> = { failed: '回复失败', rejected: '没有处理' }
const busyStates = ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling']

export function ConversationScreen() {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const [sendError, setSendError] = useState<unknown>(null)
  const [files, setFiles] = useState<File[]>([])
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const bot = workspace.selectedBot
  const text = drafts.get(conversationId) ?? ''
  const jobs = data?.scheduler?.jobs ?? []
  const activeJobs = jobs.filter((job) => busyStates.includes(job.state))
  const checks = pendingChecks(data)
  const live = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (!busyStates.includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id))))
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const agentReady = workspace.nodes.some((node) => node.online && !node.revoked && node.capabilities?.manager)
  const sendDisabled = !workspace.writable || !bot || !agentReady || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const assistantBusy = activeJobs.length > 0
  const stopJob = jobs.find((job) => data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
  const lookup = { nodes: workspace.nodes, directory }

  const pendingRow = (message: ChatMessage) => ({ key: message.message_id, anchor: message.role === 'user', node: <Message message={message} /> })
  const interruptedRow = (reply: (typeof workspace.interrupted)[number]): ChatMessage => ({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })

  // Tasks sit right after the message that started them.
  const works = data?.works ?? []
  const tasks: TaskRow[] = works.map((work) => ({ id: work.work_id, state: shownState(work), device: work.target_node, summary: work.summary, trigger: work.trigger, work }))
  const openTasks = tasks.filter((task) => taskActive(task.state) || task.state === 'unverified')
  const placed = new Set<string>()
  const taskRows = (task: TaskRow) => {
    placed.add(task.id)
    return bot ? [{ key: `task:${task.id}`, node: <TaskCard task={task} botId={bot.bot_id} device={nodeName(task.device, lookup)} open={openTasks.includes(task)} /> }] : []
  }

  // What a turn changed in the assistant shows after its reply, or after the message while no reply exists.
  const changes = data?.changes ?? []
  const placedChanges = placeChanges(data?.messages ?? [], changes)
  const changeRow = (records: AssistantChangeRecord[]) => ({ key: `change:${records[0].change_id}`, node: <ChangeCard records={records} routines={data?.assistant?.routines ?? []} /> })

  const entries: ChatEntry[] = []
  for (const message of data?.messages ?? []) {
    const routine = message.role === 'user' ? null : routineTrigger(message.in_reply_to)
    if (routine) {
      // A routine run has no message of the person; its reply stands alone and the tasks it started follow it.
      entries.push({ key: message.message_id, node: <Message message={message} routine={routineName(routine, data?.assistant, changes)} /> })
      for (const task of tasks) if (task.trigger === message.in_reply_to && !placed.has(task.id)) entries.push(...taskRows(task))
    } else entries.push(pendingRow(message))
    for (const job of jobs) {
      if (job.message_id !== message.message_id) continue
      for (const task of tasks) if (task.trigger === job.trigger) entries.push(...taskRows(task))
      if ((job.state === 'failed' || job.state === 'rejected')) entries.push({ key: `job:${job.trigger}`, node: <JobLine job={job} /> })
    }
    for (const reply of workspace.interrupted) if (reply.after === message.message_id) entries.push(pendingRow(interruptedRow(reply)))
    const turn = placedChanges.after.get(message.message_id)
    if (turn) entries.push(changeRow(turn))
  }
  for (const reply of workspace.interrupted) if (!reply.after || !data?.messages.some((message) => message.message_id === reply.after)) entries.push(pendingRow(interruptedRow(reply)))
  for (const task of tasks) if (!placed.has(task.id)) entries.push(...taskRows(task))
  for (const turn of placedChanges.rest) entries.push(changeRow(turn))
  if (messagePending && !data?.messages.some((message) => message.message_id === messagePending.message_id)) entries.push(pendingRow({ message_id: messagePending.message_id, event_hash: messagePending.message_id, role: 'user', text: messagePending.text, created_at_ms: 0, state: 'sending' }))
  for (const progress of live) entries.push(pendingRow({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0, state: 'streaming' }))

  const owner = data?.conversation?.owner
  const here = workspace.nodes.find((node) => node.device_id === data?.local_node)
  const ownerAway = Boolean(owner && here?.online && here.capabilities?.manager && owner !== here.device_id && !workspace.nodes.some((node) => node.device_id === owner && node.online && !node.revoked))
  const needsConfirm = checks.length > 0 || Boolean(messagePending && !messageBusy && !assistantBusy)
  const status = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status
  if (assistantBusy && !live.length) entries.push({ key: 'activity', node: <p role="status" className="flex items-center gap-2 text-meta text-muted-foreground"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />{status ? progressLabel(status) : '思考中'}</p> })

  // Everything waiting on the person ends the conversation, so it scrolls with it instead of covering it.
  const cards = [
    messagePending && !messageBusy ? <ActionCard key="undelivered" icon={MessageSquareWarning} title="消息可能没送达" actions={<>
      <ActionButton variant="ghost" disabled={!workspace.writable} action={() => workspace.releaseMessagePending()}>不发了</ActionButton>
      <ActionButton action={() => workspace.refresh()}>再查一次</ActionButton>
      <ActionButton variant="default" disabled={!workspace.writable || workspace.incomplete} action={async () => {
        if (await workspace.sendMessage(messagePending.text) && workspace.isCurrent()) clearSentDraft(messagePending.conversation_id, messagePending.text)
      }}>重新发送</ActionButton>
    </>}><p className="line-clamp-2 text-muted-foreground">{messagePending.text}</p></ActionCard> : null,
    ownerAway && owner ? <ActionCard key="owner" icon={ArrowRightLeft} title={`${nodeName(owner, lookup)} 离线了，这个对话在等它`} actions={
      <ActionButton variant="default" disabled={!workspace.writable} action={() => workspace.takeOver()}>交给这台设备处理</ActionButton>
    }><p className="text-muted-foreground">交给这台设备后，排队的消息在这里继续回复。</p></ActionCard> : null,
    ...checks.map((check) => <ConfirmCard key={check.check_id} check={check} />),
    // A decision a running task of this chat waits on shows here too, not only in 待处理.
    // ponytail: matched by node (and cwd when the alert has one); match thread_id if works ever carry it.
    ...workspace.alerts.filter((alert) => alert.resolution === null && works.some((work) => !work.outcome && work.target_node === alert.node && (!alert.cwd || work.cwd === alert.cwd)))
      .map((alert) => <AlertCard key={alert.alert_id} alert={alert} />),
  ].filter(Boolean)
  if (cards.length) entries.push({ key: 'waiting-on-you', node: <div id="chat-waiting" className="grid scroll-mb-4 gap-3">{cards}</div> })

  const managerOffline = workspace.nodes.some((node) => !node.revoked && !node.online && node.capabilities?.manager)
  const hint = !workspace.connectionOnline ? '离线，恢复后可发送'
    : workspace.readOnlyNotice ? workspace.readOnlyNotice
    : workspace.writeNotice ? '同步中，稍后可发送'
    : messagePending ? '上一条消息还没确认送达'
    : needsConfirm ? '先处理上面这件事'
    : bot && !agentReady ? (managerOffline ? '能运行助手的设备离线了' : '暂时没有能运行助手的设备')
    : bot ? composerHint(gate) : ''
  const needsSetup = Boolean(bot && !agentReady)

  useEffect(() => {
    if (messagePending) sentDrafts.current.set(messagePending.conversation_id, messagePending)
    const previous = sentDrafts.current.get(conversationId)
    if (!previous || !data?.messages.some((message) => message.message_id === previous.message_id)) return
    sentDrafts.current.delete(conversationId)
    setDrafts((current) => {
      if (current.get(conversationId)?.trim() !== previous.text) return current
      const next = new Map(current)
      next.delete(conversationId)
      return next
    })
  }, [conversationId, data, messagePending])
  useEffect(() => { setSendError(null) }, [conversationId])

  function clearSentDraft(id: string, sent: string) {
    setDrafts((current) => {
      if (current.get(id)?.trim() !== sent.trim()) return current
      const next = new Map(current)
      next.delete(id)
      return next
    })
  }
  const setText = (value: string) => setDrafts((current) => new Map(current).set(conversationId, value))

  if (bot && !data) return <section className="conversation-screen" aria-label="对话"><ChatSkeleton /></section>

  return (
    <section className="conversation-screen" aria-label="对话">
      <Chat
        key={conversationId}
        entries={entries}
        empty={bot ? <div className="mx-auto my-auto grid max-w-md justify-items-center gap-4 px-4 py-12 text-center">
          <Bot aria-hidden="true" className="size-8 text-muted-foreground" />
          <h2 className="text-heading font-semibold">{bot.name}</h2>
          <div className="flex flex-wrap justify-center gap-2">
            {examples.map((example) => <Button key={example} type="button" variant="outline" className="rounded-full" onClick={() => setText(example)}>{example}</Button>)}
          </div>
        </div> : null}
      />
      <PromptInput
        value={text}
        hint={hint}
        disabled={sendDisabled}
        busy={messageBusy}
        onStop={stopJob && workspace.writable && bot ? () => { setSendError(null); workspace.cancelManager(stopJob.trigger).catch(setSendError) } : undefined}
        onChange={setText}
        files={files}
        onFiles={setFiles}
        onSubmit={() => {
          if (sendDisabled || !text.trim()) return
          setSendError(null)
          workspace.sendMessage(text, files).then((sent) => { if (sent && workspace.isCurrent()) { clearSentDraft(conversationId, text); setFiles([]) } }).catch(setSendError)
        }}
      >
        <InlineError error={sendError} />
        {cards.length ? <button type="button" className="w-fit font-medium text-foreground underline underline-offset-4" onClick={() => document.getElementById('chat-waiting')?.scrollIntoView({ block: 'end', behavior: 'smooth' })}>去处理</button> : null}
        {needsSetup ? <Link to="/devices" className="w-fit font-medium text-foreground underline underline-offset-4">去配置</Link> : null}
      </PromptInput>
    </section>
  )
}

type TaskRow = { id: string; state: string; device: string; summary: string; trigger: string; work: WorkRecord }

/** Running tasks get a full card; finished ones shrink to one line that still opens the details. */
function TaskCard({ task, botId, device, open }: { task: TaskRow; botId: string; device: string; open: boolean }) {
  const failed = task.state === 'failed' || task.state === 'interrupted' || task.state === 'rejected' || task.state === 'dispatch_rejected'
  const result = (task.work.result_text || task.work.failure_reason || '').split('\n')[0]
  const title = task.work.title || '任务'
  // The reason wraps on its own line, so a phone shows why a task stopped.
  if (!open) return <Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} className="flex min-h-9 w-fit max-w-full items-start gap-2 rounded-lg px-2 py-2 text-meta text-muted-foreground hover:bg-muted/60 hover:text-foreground">
    {task.state === 'abandoned' ? <Ban aria-hidden="true" className="size-4 shrink-0" /> : failed ? <CircleX aria-hidden="true" className="size-4 shrink-0" /> : <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />}
    <span className="min-w-0"><span className="flex min-w-0 flex-col sm:flex-row sm:gap-1"><span className="truncate">{title}</span><TaskMeta device={device} state={task.state} /></span>{result ? <span className="block break-words">{result}</span> : null}</span>
    <ChevronRight aria-hidden="true" className="size-4 shrink-0" />
  </Link>
  return <Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} className="flex min-h-12 items-center gap-3 rounded-xl border bg-card px-4 py-2 hover:bg-muted/50">
    <ListChecks aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />
    <span className="min-w-0 flex-1"><span className="flex min-w-0 flex-col sm:flex-row sm:items-baseline sm:gap-1"><span className="truncate font-medium">{title}</span><TaskMeta device={device} state={task.state} className="text-meta sm:text-base" /></span>{task.summary ? <span className="block truncate text-meta text-muted-foreground">{task.summary}</span> : null}</span>
    <span className="flex shrink-0 items-center text-meta text-muted-foreground">详情<ChevronRight aria-hidden="true" className="size-4" /></span>
  </Link>
}

/** Device and state never yield to a long title: below `sm` they take their own line. */
function TaskMeta({ device, state, className = '' }: { device: string; state: string; className?: string }) {
  return <span className={`flex min-w-0 shrink-0 items-center gap-1 text-muted-foreground ${className}`}>
    <span aria-hidden="true" className="hidden sm:inline">·</span>
    <Laptop aria-hidden="true" className="size-3.5 shrink-0" />
    <span className="truncate">{device}</span>
    <span aria-hidden="true">·</span>
    <span className="shrink-0">{taskLabel(state)}</span>
  </span>
}

function JobLine({ job }: { job: ManagerJob }) {
  return <p className="flex min-w-0 items-center gap-2 px-2 text-meta text-danger">
    <CircleX aria-hidden="true" className="size-4 shrink-0" />
    <span className="shrink-0">{replyStates[job.state] ?? '回复失败'}</span>
    {job.error ? <span className="min-w-0 truncate" title={job.error}>{humanError(job.error.split('\n')[0], undefined, '这次回复')}</span> : null}
  </p>
}
