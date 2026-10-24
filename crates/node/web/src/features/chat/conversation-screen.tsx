import { useEffect, useRef, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ArrowDownToLine, ArrowRightLeft, Ban, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Forward, Laptop, Link2, ListChecks, LoaderCircle, MessageSquareReply, MessageSquareWarning, Send, Server, Unplug, WifiOff } from 'lucide-react'
import { ActionCard, Details, InlineError } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { useDevice } from '@/features/identity/context'
import { useWorkspace } from '@/features/workspace/context'
import { humanError } from '@/lib/errors'
import { terms } from '@/lib/glossary'
import { ActionButton } from '@/features/workspace/present'
import { ChatSkeleton } from '@/features/workspace/workspace-shell'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { QuestionCard } from '@/features/workspace/question-card'
import { nodeName, progressLabel, shownState, taskActive, taskLabel } from '@/features/workspace/labels'
import type { BotChangeRecord, ManagerJob, SentBotMessage, SessionHandoff, WorkRecord } from '@/features/workspace/types'
import { placeChanges, scheduleName, scheduleTrigger } from '@/features/workspace/bot-settings'
import { pendingChecks } from '@/features/workspace/model'
import { agentName, foldAnswers, placeByTime, projectName } from '@/features/workspace/questions'
import { ChangeCard } from './change-card'
import { Chat, type ChatEntry } from './elements/chat'
import type { ChatMessage } from './elements/message'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint, threadReplies } from './protocol'
import './chat.css'

const examples = ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？']
const replyStates: Record<string, string> = { failed: '回复失败', rejected: '没有处理' }
const busyStates = ['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling']
// The owner wrote the reply but it has not synced here yet; its streamed text stands in until it lands.
const streamingStates = [...busyStates, 'publishing']

export function ConversationScreen() {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const [sendError, setSendError] = useState<unknown>(null)
  const [files, setFiles] = useState<File[]>([])
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const bot = workspace.selectedBot
  const text = drafts.get(conversationId) ?? ''
  const jobs = data?.scheduler?.jobs ?? []
  const unanswered = data?.scheduler?.unanswered ?? []
  const activeJobs = jobs.filter((job) => busyStates.includes(job.state))
  const checks = pendingChecks(data, workspace.nodes, conversationId)
  const live = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (!streamingStates.includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id))))
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const agentReady = workspace.nodes.some((node) => node.online && !node.revoked && node.capabilities?.manager)
  const sendDisabled = !workspace.writable || !bot || !agentReady || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const botBusy = activeJobs.length > 0
  const stopJob = jobs.find((job) => data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
  const lookup = { nodes: workspace.nodes, directory }

  const pendingRow = (message: ChatMessage, retry?: () => Promise<void>): ChatEntry => ({ key: message.message_id, anchor: message.role === 'user', message, retry })
  const interruptedRow = (reply: (typeof workspace.interrupted)[number]): ChatMessage => ({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })

  // Tasks sit right after the message that started them.
  const works = data?.works ?? []
  // A running turn paused on an approval reads as waiting; a steered input names the task it joined.
  const asking = new Set((data?.questions ?? []).filter((question) => !question.answer && question.about.request_id).map((question) => question.about.session_id))
  const tasks: TaskRow[] = works.map((work, index) => {
    const state = shownState(work, workspace.nodes)
    const joined = work.turn_id ? works.slice(0, index).find((item) => item.thread_id === work.thread_id && item.turn_id === work.turn_id) : undefined
    return { id: work.work_id, state: state === 'running' && work.thread_id && asking.has(work.thread_id) ? 'waiting' : state, device: work.target_node, summary: work.summary, trigger: work.trigger, work, joined: joined?.title }
  })
  const openTasks = tasks.filter((task) => taskActive(task.state) || task.state === 'waiting' || task.state === 'awaiting_confirmation')
  const placed = new Set<string>()
  const taskRows = (task: TaskRow) => {
    placed.add(task.id)
    return bot ? [{ key: `task:${task.id}`, node: <TaskCard task={task} botId={bot.bot_id} device={nodeName(task.device, lookup)} here={task.device === ready?.entry.device_id} open={openTasks.includes(task)} live={workspace.connectionOnline} /> }] : []
  }

  // What a turn changed in the bot shows after its reply, or after the message while no reply exists.
  const changes = data?.changes ?? []
  const placedChanges = placeChanges(data?.messages ?? [], changes)
  const changeRow = (records: BotChangeRecord[]) => ({ key: `change:${records[0].change_id}`, node: <ChangeCard records={records} schedules={data?.bot_settings?.schedules ?? []} /> })

  // Questions, hand-offs and messages to other bots sit where they happened as rows, not bubbles; an answer shows inside its question.
  const questions = data?.questions ?? []
  const folded = foldAnswers(data?.messages ?? [], questions)
  const asides = placeByTime(data?.messages ?? [], [
    ...questions.map((question) => ({ key: `question:${question.question_id}`, created_at_ms: question.created_at_ms, node: <div id={`question-${question.question_id}`} className="scroll-mb-4"><QuestionCard question={question} answer={folded.answers.get(question.question_id)} /></div> })),
    ...(data?.sessions ?? []).map((session) => ({ key: `session:${session.node}:${session.session_id}`, created_at_ms: session.created_at_ms, node: <HandoffRow session={session} owner={session.conversation_id === conversationId ? null : workspace.bots.find((bot) => bot.conversation_id === session.conversation_id) ?? { name: '另一个 Bot' }} /> })),
    ...(data?.sent ?? []).map((sent) => ({ key: `sent:${sent.message_id}`, created_at_ms: sent.created_at_ms, node: <SentToBot sent={sent} to={workspace.bots.find((bot) => bot.conversation_id === sent.to) ?? { name: '另一个 Bot' }} /> })),
  ])

  // When the newest reply failed, its message can go out again as is, unless it carried files.
  const newest = data?.messages.at(-1)
  const failedAsk = newest?.status === 'failed' ? data?.messages.find((message) => message.role === 'user' && message.message_id === newest.in_reply_to && !message.attachments?.length) : undefined
  const retry = failedAsk && !sendDisabled ? async () => { setSendError(null); await workspace.sendMessage(failedAsk.text) } : undefined
  const replied = new Set((data?.messages ?? []).map((message) => message.message_id))
  // A message sent while the bot replies waits behind that reply, so it shows after it until its own turn starts.
  const replying = jobs.some((job) => ['claimed', 'running', 'cancelling'].includes(job.state))
  const queued = new Set(replying ? jobs.filter((job) => job.message_id && ['pending_delivery', 'queued', 'steering'].includes(job.state)).map((job) => job.message_id) : [])
  // Steering already went into the running reply; it only settles once that reply ends.
  const merging = new Set(jobs.filter((job) => job.state === 'steering').map((job) => job.message_id))
  const merged = new Map(jobs.filter((job) => job.state === 'steered').map((job) => [job.message_id, job.outcome]))
  const entries: ChatEntry[] = []
  const waiting: ChatEntry[] = []
  for (const message of threadReplies(data?.messages ?? [], merged)) {
    const start = entries.length
    for (const aside of asides.before.get(message.message_id) ?? []) entries.push(aside)
    const schedule = message.role === 'assistant' ? scheduleTrigger(message.in_reply_to) : null
    if (schedule) {
      // A schedule run has no message of the person; its reply stands alone and the tasks it started follow it.
      entries.push({ key: message.message_id, message, schedule: scheduleName(schedule, data?.bot_settings, changes) })
      for (const task of tasks) if (task.trigger === message.in_reply_to && !placed.has(task.id)) entries.push(...taskRows(task))
    } else if (!folded.hidden.has(message.message_id)) entries.push(pendingRow(merged.has(message.message_id) ? { ...message, state: 'merged', merged: merged.get(message.message_id) } : message, message.message_id === newest?.message_id ? retry : undefined))
    const cause = jobs.find((job) => job.reply_message_id === message.message_id)?.error
    if (cause) entries.push({ key: `cause:${message.message_id}`, node: <ReplyCause raw={cause} /> })
    for (const job of jobs) {
      if (job.message_id !== message.message_id) continue
      for (const task of tasks) if (task.trigger === job.trigger) entries.push(...taskRows(task))
      if ((job.state === 'failed' || job.state === 'rejected') && !replied.has(job.reply_message_id ?? '')) entries.push({ key: `job:${job.trigger}`, node: <JobLine job={job} /> })
    }
    for (const reply of workspace.interrupted) if (reply.after === message.message_id) entries.push(pendingRow(interruptedRow(reply)))
    const turn = placedChanges.after.get(message.message_id)
    if (turn) entries.push(changeRow(turn))
    if (queued.has(message.message_id)) waiting.push(...entries.splice(start).map((entry) => 'message' in entry && entry.message.message_id === message.message_id ? { ...entry, message: { ...entry.message, state: merging.has(message.message_id) ? 'merging' as const : 'queued' as const } } : entry))
  }
  entries.push(...asides.rest)
  for (const reply of workspace.interrupted) if (!reply.after || !data?.messages.some((message) => message.message_id === reply.after)) entries.push(pendingRow(interruptedRow(reply)))
  for (const task of tasks) if (!placed.has(task.id)) entries.push(...taskRows(task))
  for (const turn of placedChanges.rest) entries.push(changeRow(turn))
  if (messagePending && !data?.messages.some((message) => message.message_id === messagePending.message_id)) entries.push(pendingRow({ message_id: messagePending.message_id, event_hash: messagePending.message_id, role: 'user', text: messagePending.text, created_at_ms: 0, state: 'sending' }))
  for (const progress of live) entries.push(pendingRow({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0, state: 'streaming' }))

  const owner = data?.conversation?.owner
  const here = workspace.nodes.find((node) => node.device_id === data?.local_node)
  const ownerAway = Boolean(owner && here?.online && here.capabilities?.manager && owner !== here.device_id && !workspace.nodes.some((node) => node.device_id === owner && node.online && !node.revoked))
  const lastUnanswered = data?.messages.filter((message) => unanswered.includes(message.message_id)).at(-1)
  const status = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status
  // The owner holds the reply's state; when it does not answer, a waiting message shows that state as unknown.
  const remote = data?.scheduler?.remote
  if (remote && !remote.reachable && !ownerAway && newest?.role === 'user') entries.push({ key: 'owner-unknown', node: <p role="status" title={nodeName(remote.owner, lookup)} className="flex min-h-7 items-center gap-2 text-meta text-muted-foreground"><Unplug aria-hidden="true" className="size-4" />状态未知</p> })
  if (botBusy && !live.length) entries.push({ key: 'activity', node: <p role="status" className="flex min-h-7 items-center gap-2 text-meta text-muted-foreground"><LoaderCircle aria-hidden="true" className={`size-4 ${workspace.connectionOnline ? 'animate-spin' : ''}`} />{status ? progressLabel(status) : '思考中'}</p> })
  entries.push(...waiting)

  // Everything waiting on the person ends the conversation, so it scrolls with it instead of covering it.
  const cards = [
    messagePending && !messageBusy ? <ActionCard key="undelivered" icon={MessageSquareWarning} title="消息可能没送达" actions={<>
      <ActionButton variant="ghost" disabled={!workspace.writable} action={() => workspace.releaseMessagePending()}>不发了</ActionButton>
      <ActionButton action={() => workspace.refresh()}>再查一次</ActionButton>
      <ActionButton variant="default" disabled={!workspace.writable || workspace.incomplete} action={async () => {
        if (await workspace.sendMessage(messagePending.text) && workspace.isCurrent()) clearSentDraft(messagePending.conversation_id, messagePending.text)
      }}>重新发送</ActionButton>
    </>}><p className="line-clamp-2 text-muted-foreground">{messagePending.text}</p></ActionCard> : null,
    ownerAway && owner && here ? <ActionCard key="owner" icon={WifiOff} title={`${nodeName(owner, lookup)} 离线，对话在等它`} actions={
      <ActionButton variant="default" disabled={!workspace.writable} action={() => workspace.takeOver()}><ArrowRightLeft aria-hidden="true" />换到 {nodeName(here.device_id, lookup)}</ActionButton>
    } /> : null,
    unanswered.length ? <ActionCard key="unanswered" icon={MessageSquareReply} title={unanswered.length > 1 ? `${unanswered.length} 条消息没等到回复` : '这条消息没等到回复'} actions={<>
      <ActionButton variant="ghost" disabled={!workspace.writable} action={() => workspace.settleUnanswered(unanswered, 'dismiss')}>不回复</ActionButton>
      <ActionButton variant="default" disabled={!workspace.writable || !agentReady} action={() => workspace.settleUnanswered(unanswered, 'reply')}>{here ? <><Laptop aria-hidden="true" />由 {nodeName(here.device_id, lookup)} 回复</> : '现在回复'}</ActionButton>
    </>}>{lastUnanswered ? <p className="line-clamp-2 text-muted-foreground">{lastUnanswered.text}</p> : null}</ActionCard> : null,
    ...checks.map((check) => <ConfirmCard key={check.check_id} check={check} />),
  ].filter(Boolean)
  if (cards.length) entries.push({ key: 'waiting-on-you', node: <div id="chat-waiting" className="grid scroll-mb-4 gap-3">{cards}</div> })

  const managerOffline = workspace.nodes.some((node) => !node.revoked && !node.online && node.capabilities?.manager)
  const notice = !workspace.connectionOnline ? '离线，恢复后可发送'
    : workspace.readOnlyNotice ? workspace.readOnlyNotice
    : workspace.writeNotice ? '同步中，稍后可发送'
    : messagePending ? '上一条消息还没确认送达' : ''
  // Questions sit where they were asked, so an older one scrolls away; 去处理 brings back the first still open.
  const asked = questions.find((question) => !question.answer && !folded.answers.has(question.question_id) && !(question.question_id in workspace.answered))
  const toCards = () => document.getElementById(asked ? `question-${asked.question_id}` : 'chat-waiting')?.scrollIntoView({ block: asked ? 'center' : 'end', behavior: 'smooth' })
  const hint = notice || (bot && !agentReady ? (managerOffline ? '能运行 Bot 的设备离线了' : '暂时没有能运行 Bot 的设备')
    : bot ? composerHint(gate) : '')
  const needsSetup = Boolean(bot && !agentReady)
  const setupTo = managerOffline || !ready ? '/devices' : '/devices/$deviceId'

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
          {needsSetup ? <Button asChild><Link to={setupTo} params={{ deviceId: ready?.entry.device_id ?? '' }}><Bot aria-hidden="true" />{managerOffline ? '查看设备' : '设置运行 Bot 的设备'}</Link></Button>
            : <div className="flex flex-wrap justify-center gap-2">
              {examples.map((example) => <Button key={example} type="button" variant="outline" className="rounded-full" onClick={() => { setText(example); document.getElementById('message-text')?.focus() }}>{example}</Button>)}
            </div>}
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
        {cards.length || asked ? <button type="button" className="inline-flex w-fit items-center font-medium text-foreground underline underline-offset-4 max-md:min-h-11" onClick={toCards}>去处理</button> : null}
        {needsSetup && entries.length ? <Link to={setupTo} params={{ deviceId: ready?.entry.device_id ?? '' }} className="w-fit font-medium text-foreground underline underline-offset-4">去配置</Link> : null}
      </PromptInput>
    </section>
  )
}

type TaskRow = { id: string; state: string; device: string; summary: string; trigger: string; work: WorkRecord; joined?: string }

/** Running tasks get a full card; finished ones shrink to one line that still opens the details. */
function TaskCard({ task, botId, device, here, open, live }: { task: TaskRow; botId: string; device: string; here: boolean; open: boolean; live: boolean }) {
  const failed = task.state === 'failed' || task.state === 'rejected' || task.state === 'dispatch_rejected'
  const result = (task.work.result_text || (task.work.failure_reason ? humanError(task.work.failure_reason, undefined, '这个任务') : '')).split('\n')[0]
  const title = task.work.title || '任务'
  // The reason wraps on its own line, so a phone shows why a task stopped.
  if (!open) return <Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} className="-mx-2 flex min-h-9 w-fit max-w-full items-start gap-2 rounded-lg px-2 py-2 text-meta text-muted-foreground hover:bg-muted/60 hover:text-foreground">
    {task.state === 'abandoned' ? <Ban aria-hidden="true" className="size-4 shrink-0" /> : task.state === 'interrupted' ? <CircleStop aria-hidden="true" className="size-4 shrink-0" /> : failed ? <CircleX aria-hidden="true" className="size-4 shrink-0 text-danger" /> : <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />}
    <span className="min-w-0"><span className="flex min-w-0 flex-col sm:flex-row sm:gap-1"><span className="truncate">{title}</span><TaskMeta device={device} here={here} state={task.state} /></span>{result ? <span className="block break-words">{result}</span> : null}</span>
    <ChevronRight aria-hidden="true" className="size-4 shrink-0" />
  </Link>
  return <Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} className="flex min-h-12 items-center gap-3 rounded-xl border bg-card px-4 py-2 hover:bg-muted/50">
    {task.state === 'waiting' ? <terms.approval.icon aria-hidden="true" className="size-5 shrink-0 text-warning" />
      : task.state === 'awaiting_confirmation' ? <CircleQuestionMark aria-hidden="true" className="size-5 shrink-0 text-warning" />
      : taskActive(task.state) ? <LoaderCircle aria-hidden="true" className={`size-5 shrink-0 text-info ${live ? 'animate-spin' : ''}`} /> : <ListChecks aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />}
    <span className="min-w-0 flex-1"><span className="flex min-w-0 flex-col sm:flex-row sm:items-baseline sm:gap-1"><span className="truncate font-medium">{title}</span><TaskMeta device={device} here={here} state={task.state} className="text-meta" /></span>
      {task.joined ? <span className="flex min-w-0 items-center gap-1 text-meta text-muted-foreground"><Link2 aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">并入「{task.joined}」</span></span>
        : taskActive(task.state) && task.summary ? <span className="block truncate text-meta text-muted-foreground">{task.summary}</span> : null}</span>
    <ChevronRight aria-label="详情" className="size-4 shrink-0 text-muted-foreground" />
  </Link>
}

/** Device and state never yield to a long title: below `sm` they take their own line. */
function TaskMeta({ device, here, state, className = '' }: { device: string; here: boolean; state: string; className?: string }) {
  const Icon = here ? Laptop : Server
  return <span className={`flex min-w-0 shrink-0 items-center gap-1 text-muted-foreground ${className}`}>
    <span aria-hidden="true" className="hidden sm:inline">·</span>
    <Icon aria-hidden="true" className="size-3.5 shrink-0" />
    <span className="truncate">{device}</span>
    <span aria-hidden="true">·</span>
    <span className="shrink-0">{taskLabel(state)}</span>
  </span>
}

/** A session handed to this bot, or by this primary bot to another one. */
function HandoffRow({ session, owner }: { session: SessionHandoff; owner: { name: string; bot_id?: string } | null }) {
  const Icon = owner ? Forward : ArrowDownToLine
  return <p className="flex min-h-9 min-w-0 items-center gap-2 text-meta text-muted-foreground">
    <Icon aria-hidden="true" className="size-4 shrink-0" />
    {owner ? <span className="shrink-0 text-foreground">{agentName(session.agent)} 会话交给了 {owner.bot_id
      ? <Link to="/b/$botId" params={{ botId: owner.bot_id }} className="font-medium underline-offset-4 hover:underline">{owner.name}</Link> : owner.name}</span>
      : <span className="shrink-0 text-foreground">接手 {agentName(session.agent)} 会话</span>}
    <span aria-hidden="true">·</span>
    <span className="min-w-0 max-w-[40%] shrink truncate" title={session.cwd}><span className="sr-only">项目 </span>{projectName(session.cwd)}</span>
    {session.summary ? <><span aria-hidden="true">·</span><span className="min-w-0 flex-1 truncate" title={session.summary}>{session.summary}</span></> : null}
  </p>
}

function SentToBot({ sent, to }: { sent: SentBotMessage; to: { name: string; bot_id?: string } }) {
  return <div className="grid min-w-0 gap-1 rounded-lg border px-3 py-2 text-meta">
    <p className="flex min-w-0 items-center gap-2 text-muted-foreground">
      <Send aria-hidden="true" className="size-4 shrink-0" />
      <span className="shrink-0">发给</span>
      {to.bot_id ? <Link to="/b/$botId" params={{ botId: to.bot_id }} className="min-w-0 truncate font-medium text-foreground underline-offset-4 hover:underline">{to.name}</Link>
        : <span className="min-w-0 truncate font-medium text-foreground">{to.name}</span>}
    </p>
    <p className="line-clamp-3 whitespace-pre-wrap text-foreground [overflow-wrap:anywhere]" title={sent.text}>{sent.text}</p>
  </div>
}

/** The provider's own words behind a failed reply, folded under it. */
function ReplyCause({ raw }: { raw: string }) {
  return <Details className="-mt-2">{raw}</Details>
}

function JobLine({ job }: { job: ManagerJob }) {
  return <p className="flex min-w-0 items-center gap-2 text-meta text-danger">
    <CircleX aria-hidden="true" className="size-4 shrink-0" />
    <span className="shrink-0">{replyStates[job.state] ?? '回复失败'}</span>
    {job.error ? <span className="min-w-0 truncate" title={job.error}>{humanError(job.error.split('\n')[0], undefined, '这次回复')}</span> : null}
  </p>
}
