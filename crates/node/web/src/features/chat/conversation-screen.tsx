import { useEffect, useRef, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { Bot, ChevronDown, ChevronRight, CircleCheck, ListChecks, LoaderCircle, MessageSquareWarning } from 'lucide-react'
import { ActionCard, InlineError } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { useDevice } from '@/features/identity/context'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton } from '@/features/workspace/present'
import { ChatSkeleton } from '@/features/workspace/workspace-shell'
import { ApprovalCard, ConfirmCard } from '@/features/workspace/task-activity'
import { approvalKind, nodeName, taskActive, taskLabel } from '@/features/workspace/labels'
import type { AutonomyMode, ManagerJob } from '@/features/workspace/types'
import { Chat } from './elements/chat'
import type { ChatMessage } from './elements/message'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import './chat.css'

const examples = ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？']
const replyStates: Record<string, string> = {
  pending_delivery: '等待中', queued: '等待中', claimed: '准备中', running: '回复中', cancelling: '停止中',
  cancelled: '已停止', completed: '已回复', failed: '回复失败', rejected: '没有处理', interrupted: '已中断',
}
const busyStates = ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling']

export function ConversationScreen() {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const [sendError, setSendError] = useState<unknown>(null)
  const [timelineOpen, setTimelineOpen] = useState(false)
  const [allApprovals, setAllApprovals] = useState(false)
  // ponytail: answered approvals stay for this page view only; persist like interrupted replies if people miss them.
  const [answered, setAnswered] = useState<{ key: string; text: string; at: number }[]>([])
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const bot = workspace.selectedBot
  const text = drafts.get(conversationId) ?? ''
  const originalEntry = !messagePending || messagePending.entry_node === workspace.info.peer_id
  const jobs = data?.scheduler?.jobs ?? []
  const activeJobs = jobs.filter((job) => busyStates.includes(job.state))
  const pendingChecks = data?.pending_checks ?? []
  const live = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (!busyStates.includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id))))
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const agentReady = workspace.nodes.some((node) => node.online && !node.revoked && node.capabilities?.manager)
  const sendDisabled = !workspace.writable || !bot || !agentReady || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const assistantBusy = activeJobs.length > 0
  const stopJob = jobs.find((job) => data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
  const lookup = { nodes: workspace.nodes, directory }

  const shown: ChatMessage[] = []
  for (const message of data?.messages ?? []) {
    shown.push(message)
    for (const reply of workspace.interrupted) if (reply.after === message.message_id) shown.push({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })
  }
  for (const reply of workspace.interrupted) if (!reply.after || !data?.messages.some((message) => message.message_id === reply.after)) shown.push({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })
  if (messagePending && !shown.some((message) => message.message_id === messagePending.message_id)) shown.push({ message_id: messagePending.message_id, event_hash: messagePending.message_id, role: 'user', text: messagePending.text, created_at_ms: 0, state: 'sending' })
  for (const progress of live) shown.push({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0, state: 'streaming' })

  const tasks: { id: string; state: string; device: string; summary: string }[] = (data?.agent_tasks ?? []).map((task) => ({ id: task.task_ref, state: task.state as string, device: task.target_node, summary: task.text }))
    .concat((data?.native_sessions ?? []).filter((session) => !session.task_ref || !data?.agent_tasks.some((task) => task.task_ref === session.task_ref)).map((session) => ({ id: session.work_id, state: session.state, device: session.target_node, summary: session.detail })))
  const openTasks = tasks.filter((task) => taskActive(task.state) || task.state === 'pending_check' || task.state === 'unknown')
  const doneTasks = tasks.length - openTasks.length
  const approvals = (data?.native_sessions ?? []).flatMap((session) => session.pending_requests.map((request) => ({ session, request, key: `${session.target_node}:${session.work_id}:${request.request_id}` })))
  const needsConfirm = pendingChecks.length > 0 || Boolean(messagePending && !messageBusy && !assistantBusy)
  const status = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status
  const activity = status && assistantBusy ? status : assistantBusy ? (live.length ? '正在回复' : '思考中') : openTasks.length ? '任务进行中' : ''
  const hasTimeline = assistantBusy || openTasks.length > 0 || doneTasks > 0 || jobs.some((job) => job.state === 'failed' || job.state === 'rejected')

  // Everything waiting on the person, pinned above the composer; the first shows, the rest fold into "还有 N 条".
  const cards = [
    messagePending && !messageBusy ? <ActionCard key="undelivered" icon={MessageSquareWarning} title="消息可能没送达" actions={<>
      <ActionButton variant="ghost" disabled={!workspace.writable} action={() => workspace.releaseMessagePending()}>不发了</ActionButton>
      <ActionButton action={() => workspace.refresh()}>再查一次</ActionButton>
      <ActionButton variant="default" disabled={!workspace.writable || !originalEntry || workspace.incomplete} action={async () => {
        if (await workspace.sendMessage(messagePending.text) && workspace.isCurrent()) clearSentDraft(messagePending.conversation_id, messagePending.text)
      }}>重新发送</ActionButton>
    </>}><p className="line-clamp-2 text-muted-foreground">{messagePending.text}</p></ActionCard> : null,
    ...pendingChecks.map((check) => <ConfirmCard key={`${check.target_node}:${check.check_id}`} check={check} />),
    ...approvals.map(({ session, request, key }) => <ApprovalCard key={key} session={session} request={request} onAnswered={(result) => setAnswered((list) => [...list, { key, text: `${result} · ${approvalKind(request)}${request.command ? ` ${request.command}` : ''}`, at: Date.now() }])} />),
  ].filter(Boolean)

  const hint = !workspace.connectionOnline ? '离线，恢复后可发送'
    : workspace.writeNotice ? '同步中，稍后可发送'
    : messagePending ? (!originalEntry ? '这条消息要回到原来的设备上处理' : '上一条消息还没确认送达')
    : needsConfirm ? '先处理上面这件事'
    : bot && !agentReady ? '暂时没有能运行助手的设备'
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
  useEffect(() => { setSendError(null); setTimelineOpen(false); setAllApprovals(false); setAnswered([]) }, [conversationId])

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
        messages={shown}
        empty={bot ? <div className="mx-auto my-auto grid max-w-md justify-items-center gap-4 px-4 py-12 text-center">
          <Bot aria-hidden="true" className="size-8 text-muted-foreground" />
          <h2 className="text-heading font-semibold">{bot.name}</h2>
          <div className="flex flex-wrap justify-center gap-2">
            {examples.map((example) => <Button key={example} type="button" variant="outline" className="rounded-full" onClick={() => setText(example)}>{example}</Button>)}
          </div>
        </div> : null}
      >
        {answered.map((item) => <p key={item.key} className="flex min-w-0 items-center gap-2 text-meta text-muted-foreground"><CircleCheck aria-hidden="true" className="size-4 shrink-0" /><span className="min-w-0 flex-1 truncate">{item.text}</span><time className="shrink-0 tabular-nums">{new Date(item.at).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })}</time></p>)}
        {assistantBusy && !live.length ? <p role="status" className="flex items-center gap-2 text-meta text-muted-foreground"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />{status ?? '思考中'}</p> : null}
        {bot ? openTasks.map((task) => <Link key={task.id} to="/b/$botId/work/$workId" params={{ botId: bot.bot_id, workId: task.id }} className="flex min-h-12 items-center gap-3 rounded-xl border bg-card px-4 py-2 hover:bg-muted/50">
          <ListChecks aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate"><span className="font-medium">任务</span><span className="text-muted-foreground"> · {nodeName(task.device, lookup)} · {taskLabel(task.state)}</span>{task.summary ? <span className="block truncate text-meta text-muted-foreground">{task.summary}</span> : null}</span>
          <span className="flex shrink-0 items-center text-meta text-muted-foreground">详情<ChevronRight aria-hidden="true" className="size-4" /></span>
        </Link>) : null}
      </Chat>
      {cards.length ? <div className="chat-activity"><div className="chat-approvals">
        {allApprovals ? cards : cards.slice(0, 1)}
        {cards.length > 1 && !allApprovals ? <button type="button" className="w-fit text-meta font-medium text-muted-foreground underline underline-offset-4 hover:text-foreground" onClick={() => setAllApprovals(true)}>还有 {cards.length - 1} 条</button> : null}
      </div></div> : null}
      {hasTimeline ? <div className="chat-activity">
        <button type="button" aria-expanded={timelineOpen} className="flex min-h-9 w-full items-center gap-2 text-meta text-muted-foreground hover:text-foreground" onClick={() => setTimelineOpen((open) => !open)}>
          {assistantBusy || openTasks.length ? <LoaderCircle aria-hidden="true" className="size-4 animate-spin" /> : <ListChecks aria-hidden="true" className="size-4" />}
          <span>{activity || '进展'}</span>
          <ChevronDown aria-hidden="true" className={`size-4 transition-transform ${timelineOpen ? 'rotate-180' : ''}`} />
        </button>
        {timelineOpen ? <ol className="grid gap-1 pb-2 text-meta">
          {jobs.slice(-8).map((job) => <JobLine key={job.trigger} job={job} />)}
          {doneTasks ? <li className="flex items-center gap-2 py-1 text-muted-foreground"><ListChecks aria-hidden="true" className="size-4" />{doneTasks} 个任务已结束
            {bot ? tasks.filter((task) => !openTasks.includes(task)).slice(-3).map((task) => <Link key={task.id} className="underline underline-offset-4" to="/b/$botId/work/$workId" params={{ botId: bot.bot_id, workId: task.id }}>{taskLabel(task.state)}</Link>) : null}
          </li> : null}
        </ol> : null}
      </div> : null}
      <PromptInput
        value={text}
        hint={hint}
        disabled={sendDisabled}
        busy={messageBusy}
        onStop={stopJob && workspace.writable && bot ? () => { setSendError(null); workspace.cancelManager(stopJob.trigger).catch(setSendError) } : undefined}
        onChange={setText}
        onSubmit={() => {
          if (sendDisabled || !text.trim()) return
          setSendError(null)
          workspace.sendMessage(text).then((sent) => { if (sent && workspace.isCurrent()) clearSentDraft(conversationId, text) }).catch(setSendError)
        }}
      >
        <InlineError error={sendError} />
        {needsSetup ? <Link to="/devices" className="w-fit font-medium text-foreground underline underline-offset-4">去配置</Link> : null}
        {data && bot ? <label className="flex w-fit items-center gap-1">
          完成后：
          <NativeSelect size="sm" aria-label="完成后" value={data.autonomy.mode} disabled={!workspace.writable || data.autonomy.version === null} onChange={(event) => {
            const mode = event.currentTarget.value as AutonomyMode
            void workspace.run(() => workspace.setAutonomy(mode))
          }}>
            <NativeSelectOption value="off">停下</NativeSelectOption>
            <NativeSelectOption value="report">只汇报</NativeSelectOption>
            <NativeSelectOption value="continue">继续做</NativeSelectOption>
          </NativeSelect>
        </label> : null}
      </PromptInput>
    </section>
  )
}

function JobLine({ job }: { job: ManagerJob }) {
  return <li className="flex items-center gap-2 py-1 text-muted-foreground">
    <span className={`size-1.5 shrink-0 rounded-full ${job.state === 'failed' || job.state === 'rejected' ? 'bg-danger' : busyStates.includes(job.state) ? 'bg-info' : 'bg-muted-foreground'}`} />
    <span className="min-w-0 flex-1 truncate">{job.message_id ? '回复你的消息' : '后台进展'} · {replyStates[job.state] ?? job.state}</span>
    {job.error ? <span className="truncate text-danger" title={job.error}>{job.error.split('\n')[0]}</span> : null}
  </li>
}
