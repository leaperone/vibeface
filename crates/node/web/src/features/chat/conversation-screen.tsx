import { useEffect, useRef, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ArrowDown, ArrowDownToLine, ArrowRightLeft, BellOff, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Crown, Laptop, Link2, ListChecks, LoaderCircle, MessageSquareOff, MessageSquareReply, Plus, RotateCcw, Send, Server, Unplug, WifiOff } from 'lucide-react'
import { ActionCard, BotAvatar, Details, EmptyState, InlineError, PrimaryMark } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { failureText, taskError } from '@vibeface/client/lib/errors.ts'
import { terms } from '@/lib/glossary'
import { ActionButton } from '@/features/workspace/present'
import { ChatSkeleton, useCreateBot } from '@/features/workspace/workspace-shell'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { QuestionCard } from '@/features/workspace/question-card'
import { nodeName, progressLabel, shownState, taskActive, taskLabel } from '@vibeface/client/workspace/labels.ts'
import type { BotChangeRecord, ManagerJob, SentBotMessage, SessionHandoff, WorkRecord } from '@vibeface/client/workspace/types.ts'
import { placeChanges, scheduleName, scheduleTrigger } from '@vibeface/client/workspace/bot-settings.ts'
import { botReadiness, pendingChecks } from '@vibeface/client/workspace/model.ts'
import { agentName, foldAnswers, placeByTime, projectName, questionOpen } from '@vibeface/client/workspace/questions.ts'
import { firstLine } from '@/lib/markdown'
import { ChangeCard } from './change-card'
import { Chat, type ChatEntry } from './elements/chat'
import type { ChatMessage } from './elements/message'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint, threadReplies } from '@vibeface/client/chat/protocol.ts'
import './chat.css'

const examples = ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？']
const busyStates = ['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling']
// The translated cause alone: an unknown one reads as its own first line, not the generic sentence above it.
const failureReason = (raw: string) => taskError(raw).split('\n').at(-1) ?? ''
// A named kind reads as its own words; otherwise the original text is all there is.
const workCause = (work: WorkRecord) => failureText(work.failure) || (work.failure_reason ? failureReason(work.failure_reason) : '')
// The owner wrote the reply but it has not synced here yet; its streamed text stands in until it lands.
const streamingStates = [...busyStates, 'publishing']

function NoBot() {
  const { bots } = useWorkspace()
  const create = useCreateBot()
  return bots.length
    ? <EmptyState icon={Bot} title="找不到这个 Bot" className="my-auto" action={<Button asChild variant="outline"><Link to="/">查看 Bot</Link></Button>} />
    : <EmptyState icon={Bot} title="还没有 Bot" className="my-auto" action={<Button type="button" onClick={create}><Plus aria-hidden="true" />新 Bot</Button>} />
}

/** `missing`: the route names a bot this network does not have. */
export function ConversationScreen({ missing = false }: { missing?: boolean }) {
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
  const agentReady = workspace.nodes.some((node) => botReadiness(node) === 'ready')
  const unready = workspace.nodes.find((node) => botReadiness(node) === 'needs_setup')
  const sendDisabled = !workspace.writable || !bot || !agentReady || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const botBusy = activeJobs.length > 0
  const stopJob = jobs.find((job) => data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
  const lookup = { nodes: workspace.nodes, directory }

  const pendingRow = (message: ChatMessage, retry?: () => Promise<void>): ChatEntry => ({ key: message.message_id, anchor: message.role === 'user', message, retry })
  const interruptedRow = (reply: (typeof workspace.interrupted)[number]): ChatMessage => ({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })

  // Tasks sit right after the message that started them.
  const works = data?.works ?? []
  // A running turn paused on an approval reads as waiting; a steered input names the task it joined.
  const asking = new Set((data?.questions ?? []).filter((question) => questionOpen(question) && question.about.request_id).map((question) => question.about.session_id))
  const tasks: TaskRow[] = works.map((work, index) => {
    const state = shownState(work, workspace.nodes)
    const joined = work.turn_id ? works.slice(0, index).find((item) => item.thread_id === work.thread_id && item.turn_id === work.turn_id) : undefined
    return { id: work.work_id, state: state === 'running' && work.thread_id && asking.has(work.thread_id) ? 'waiting' : state, device: work.target_node, summary: work.summary, trigger: work.trigger, work, joined: joined?.title }
  })
  const openTasks = tasks.filter((task) => taskActive(task.state) || task.state === 'waiting')
  const placed = new Set<string>()
  const taskRows = (task: TaskRow) => {
    placed.add(task.id)
    // A task held on an approval or waiting for confirmation shows as that card, which names and links the task.
    if (task.state === 'waiting' || task.state === 'awaiting_confirmation') return []
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
    ...(data?.sessions ?? []).map((session) => ({ key: `session:${session.node}:${session.session_id}`, created_at_ms: session.created_at_ms, node: <SessionRow session={session} /> })),
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
    const cause = jobs.find((job) => job.reply_message_id === message.message_id)?.failure?.detail
    if (cause) entries.push({ key: `cause:${message.message_id}`, node: <ReplyCause raw={cause} /> })
    for (const job of jobs) {
      if (job.message_id !== message.message_id) continue
      for (const task of tasks) if (task.trigger === job.trigger) entries.push(...taskRows(task))
      if ((job.state !== 'failed' && job.state !== 'rejected') || replied.has(job.reply_message_id ?? '')) continue
      // Only a click sends it again, as a new message; once it is sent this message is no longer the newest.
      const again = message.role === 'user' && message.message_id === newest?.message_id && !message.attachments?.length && !sendDisabled ? async () => { setSendError(null); await workspace.sendMessage(message.text) } : undefined
      entries.push({ key: `job:${job.trigger}`, node: <JobLine job={job} retry={again} /> })
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
  const ownerAway = Boolean(owner && here && botReadiness(here) === 'ready' && owner !== here.device_id && !workspace.nodes.some((node) => node.device_id === owner && node.online && !node.revoked))
  const lastUnanswered = data?.messages.filter((message) => unanswered.includes(message.message_id)).at(-1)
  const status = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status
  // The owner holds the reply's state; when it does not answer, a waiting message shows that state as unknown.
  const remote = data?.scheduler?.remote
  if (remote && !remote.reachable && !ownerAway && newest?.role === 'user') entries.push({ key: 'owner-unknown', node: <p role="status" className="flex min-h-7 items-center gap-2 text-meta text-muted-foreground"><Unplug aria-hidden="true" className="size-4 shrink-0" />{nodeName(remote.owner, lookup)} 连不上，回复情况未知</p> })
  // A question waiting on the person is the status already; a spinner under it would say the bot is still thinking.
  const asked = questions.find((question) => !question.answer && !folded.answers.has(question.question_id) && !(question.question_id in workspace.answered))
  const stopping = activeJobs.some((job) => job.state === 'cancelling')
  if (botBusy && !live.length && !asked) entries.push({ key: 'activity', node: <p role="status" className="flex min-h-7 items-center gap-2 text-meta text-muted-foreground">{stopping ? <><CircleStop aria-hidden="true" className="size-4" />正在停止…</>
    : <><LoaderCircle aria-hidden="true" className={`size-4 ${workspace.connectionOnline ? 'animate-spin' : ''}`} />{status ? progressLabel(status) : '思考中'}</>}</p> })
  entries.push(...waiting)

  // Everything waiting on the person ends the conversation, so it scrolls with it instead of covering it.
  const cards = [
    messagePending && !messageBusy ? <ActionCard key="undelivered" icon={terms.undelivered.icon} title="消息可能没送达" actions={<>
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
    : messagePending && messageBusy ? '上一条消息还没确认送达' : ''
  // Questions sit where they were asked, so an older one scrolls away; the pill above the composer brings back the first still open.
  const waitingTarget = asked ? `question-${asked.question_id}` : cards.length ? 'chat-waiting' : null
  const waitingCount = questions.filter((question) => !question.answer && !folded.answers.has(question.question_id) && !(question.question_id in workspace.answered)).length + cards.length
  const targetHidden = useOffscreen(waitingTarget, entries.length)
  // An empty chat already offers the setup button in its middle.
  const hint = notice || (bot && !agentReady ? (managerOffline && !unready ? '能运行 Bot 的设备离线了' : !entries.length ? '' : unready ? '运行 Bot 的设备需设置' : '还没设置运行 Bot 的设备')
    : bot ? composerHint(gate) : '')
  const needsSetup = Boolean(bot && !agentReady)
  const setupDevice = unready?.device_id ?? ready?.entry.device_id
  // A device that only needs setup is fixable now, so it comes before one that is offline.
  const away = managerOffline && !unready
  const setupTo = away || !setupDevice ? '/devices' : '/devices/$deviceId'
  const canSetUp = ready?.principal.can_manage === true || ready?.principal.source === 'local_session'

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

  if (missing) return <section className="conversation-screen" aria-label="对话"><Chat key="missing" entries={[]} empty={<NoBot />} /></section>
  if (bot && !data) return <section className="conversation-screen" aria-label="对话"><ChatSkeleton /></section>

  return (
    <section className="conversation-screen" aria-label="对话">
      <Chat
        key={conversationId}
        entries={entries}
        empty={bot ? <div className="mx-auto my-auto grid max-w-md justify-items-center gap-4 px-4 py-12 text-center">
          <BotAvatar name={bot.name} primary={workspace.primary === bot.conversation_id} size="lg" />
          <div className="grid justify-items-center gap-1">
            <h2 className="text-heading font-semibold">{bot.name}</h2>
            {workspace.primary === bot.conversation_id ? <PrimaryMark labeled /> : null}
          </div>
          {needsSetup ? <>
            <p className="flex items-center gap-1.5 text-muted-foreground"><MessageSquareOff aria-hidden="true" className="size-4 shrink-0" />Bot 还不能回复</p>
            {away || canSetUp ? <Button asChild><Link to={setupTo} params={{ deviceId: setupDevice ?? '' }}><Bot aria-hidden="true" />{away ? '查看设备' : '设置运行 Bot 的设备'}</Link></Button>
              : <p className="flex items-center gap-1.5 text-meta text-muted-foreground"><Crown aria-hidden="true" className="size-4 shrink-0" />请在管理设备上设置</p>}
          </>
            : <div className="flex flex-wrap justify-center gap-2">
              {examples.map((example) => <Button key={example} type="button" variant="outline" className="rounded-full text-muted-foreground hover:text-foreground" onClick={() => { setText(example); document.getElementById('message-text')?.focus() }}>{example}</Button>)}
            </div>}
        </div> : workspace.writable ? <NoBot /> : null}
      />
      {bot ? <PromptInput
        above={waitingTarget && targetHidden ? <button type="button" className="flex h-8 items-center gap-1.5 rounded-full border border-warning-border bg-card ps-2.5 pe-3 text-meta font-medium shadow-sm transition-colors hover:bg-warning-soft active:bg-warning-soft animate-in fade-in-0 slide-in-from-bottom-1 duration-200 ease-out max-md:h-11 max-md:ps-3"
          onClick={() => document.getElementById(waitingTarget)?.scrollIntoView({ block: asked ? 'center' : 'end', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' })}>
          <terms.inbox.icon aria-hidden="true" className="size-4 text-warning" />{waitingCount > 1 ? `${waitingCount} 项${terms.inbox.label}` : terms.inbox.label}<ArrowDown aria-hidden="true" className="size-3.5 text-muted-foreground" />
        </button> : null}
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
        <InlineError error={sendError} className="basis-full" />
        {needsSetup && entries.length && !notice ? <Link to={setupTo} params={{ deviceId: setupDevice ?? '' }} className="inline-flex items-center gap-0.5 font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11">去设置<ChevronRight aria-hidden="true" className="size-3.5" /></Link> : null}
      </PromptInput> : null}
    </section>
  )
}

/** Whether the element with `id` is out of view; rechecked as the list grows. */
function useOffscreen(id: string | null, revision: number) {
  const [hidden, setHidden] = useState(false)
  useEffect(() => {
    const target = id && typeof IntersectionObserver !== 'undefined' ? document.getElementById(id) : null
    if (!target) { setHidden(false); return }
    const observer = new IntersectionObserver(([entry]) => setHidden(!entry.isIntersecting))
    observer.observe(target)
    return () => observer.disconnect()
  }, [id, revision])
  return hidden
}

type TaskRow = { id: string; state: string; device: string; summary: string; trigger: string; work: WorkRecord; joined?: string }

/** Running tasks get a full card; finished ones shrink to one line that still opens the details. */
function TaskCard({ task, botId, device, here, open, live }: { task: TaskRow; botId: string; device: string; here: boolean; open: boolean; live: boolean }) {
  const failed = task.state === 'failed' || task.state === 'rejected' || task.state === 'dispatch_rejected'
  const result = task.work.result_text ? firstLine(task.work.result_text) : workCause(task.work)
  const title = task.work.title || '任务'
  // The reason wraps on its own line, so a phone shows why a task stopped.
  // Breaks by the chat column, which a task panel beside it narrows, not by the window.
  if (!open) return <div className="@container"><Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} className="-mx-2 flex min-h-9 w-fit max-w-full items-start gap-2 rounded-lg px-2 py-2 text-meta text-muted-foreground hover:bg-muted/60 hover:text-foreground active:bg-muted max-md:min-h-11 max-md:py-3">
    {task.state === 'dispatch_unverified' ? <CircleQuestionMark aria-hidden="true" className="size-4 shrink-0" /> : task.state === 'interrupted' ? <CircleStop aria-hidden="true" className="size-4 shrink-0" /> : failed ? <CircleX aria-hidden="true" className="size-4 shrink-0 text-danger" /> : <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />}
    <span className="min-w-0"><span className="flex min-w-0 flex-col @sm:flex-row @sm:gap-1"><span className="truncate">{title}</span><TaskMeta device={device} here={here} state={task.state} quiet={task.state === 'completed' || failed && Boolean(result)} /></span>{result ? <span className={`block break-words ${failed ? 'text-danger' : ''}`}>{result}</span> : null}</span>
    <ChevronRight aria-hidden="true" className="size-4 shrink-0" />
  </Link></div>
  return <div className="@container"><Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} className="flex min-h-12 items-center gap-3 interactive-row rounded-xl border bg-card shadow-xs px-4 py-2">
    {task.state === 'waiting' ? <terms.approval.icon aria-hidden="true" className="size-5 shrink-0 text-warning" />
      : taskActive(task.state) ? <LoaderCircle aria-hidden="true" className={`size-5 shrink-0 text-info ${live ? 'animate-spin' : ''}`} /> : <ListChecks aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />}
    <span className="min-w-0 flex-1"><span className="flex min-w-0 flex-col @sm:flex-row @sm:items-baseline @sm:gap-1"><span className="truncate font-medium">{title}</span><TaskMeta device={device} here={here} state={task.state} className="text-meta" /></span>
      {task.joined ? <span className="flex min-w-0 items-center gap-1 text-meta text-muted-foreground"><Link2 aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">补充到「{task.joined}」</span></span>
        : taskActive(task.state) && task.summary ? <span className="block truncate text-meta text-muted-foreground">{task.summary}</span> : null}</span>
    <ChevronRight aria-label="详情" className="size-4 shrink-0 text-muted-foreground" />
  </Link></div>
}

/** Device and state never yield to a long title: in a column narrower than `@sm` they take their own line. */
/** Only another device is named on screen. `quiet`: the icon already says the state, so its word is only read out. */
function TaskMeta({ device, here, state, quiet = false, className = '' }: { device: string; here: boolean; state: string; quiet?: boolean; className?: string }) {
  if (here && quiet) return <span className="sr-only">{device} · {taskLabel(state)}</span>
  return <span className={`flex min-w-0 shrink-0 items-center gap-1 text-muted-foreground ${className}`}>
    <span aria-hidden="true" className="hidden @sm:inline">·</span>
    {here ? <span className="sr-only">{device}</span> : <><Server aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">{device}</span>{quiet ? null : <span aria-hidden="true">·</span>}</>}
    <span className={quiet ? 'sr-only' : 'shrink-0'}>{taskLabel(state)}</span>
  </span>
}

/** A session no bot started that this bot looks after. Bots hand sessions to each other themselves; the row only shows it. */
function SessionRow({ session }: { session: SessionHandoff }) {
  const Icon = session.muted ? BellOff : ArrowDownToLine
  return <div className={`flex min-w-0 items-center gap-3 rounded-xl border border-dashed px-3 py-2 ${session.muted ? 'text-muted-foreground' : ''}`}>
    <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><Icon aria-hidden="true" className="size-4" /></span>
    <span className="grid min-w-0 flex-1">
      <span className="truncate text-meta font-medium">{session.muted ? '不再关注' : '接手'} {agentName(session.agent)} 会话</span>
      <span className="truncate text-meta text-muted-foreground" title={[session.cwd, session.summary].filter(Boolean).join('\n')}>
        <span className="sr-only">项目 </span>{projectName(session.cwd)}{session.summary ? ` · ${session.summary}` : ''}
      </span>
    </span>
  </div>
}

/** Shaped like a message from another bot, its name above the text, so both directions read as one kind of thing. */
function SentToBot({ sent, to }: { sent: SentBotMessage; to: { name: string; bot_id?: string } }) {
  return <div className="flex min-w-0 flex-col items-start gap-1">
    <p className="flex min-w-0 max-w-full items-center gap-1.5 text-meta text-muted-foreground">
      <Send aria-hidden="true" className="size-4 shrink-0" />
      <span className="shrink-0">发给</span>
      {to.bot_id ? <Link to="/b/$botId" params={{ botId: to.bot_id }} className="hit-area relative flex min-w-0 font-medium text-foreground underline-offset-4 hover:underline"><span className="truncate">{to.name}</span></Link>
        : <span className="min-w-0 truncate font-medium text-foreground">{to.name}</span>}
    </p>
    <p className="line-clamp-3 min-w-0 max-w-[85%] whitespace-pre-wrap rounded-2xl border px-4 py-2 [overflow-wrap:anywhere] sm:max-w-[75%]" title={sent.text}>{sent.text}</p>
  </div>
}

/** The provider's own words behind a failed reply, folded under it. */
function ReplyCause({ raw }: { raw: string }) {
  return <Details className="-mt-2">{raw}</Details>
}

/** `retry`: sends the message again as a new one; only offered while it is the newest. */
function JobLine({ job, retry }: { job: ManagerJob; retry?: () => Promise<void> }) {
  return <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta">
    <span className="flex shrink-0 items-center gap-1 text-danger"><CircleX aria-hidden="true" className="size-4 shrink-0" />Bot 没能回复</span>
    {job.failure ? <span className="min-w-0 truncate text-muted-foreground" title={job.failure.detail}>{failureText(job.failure.kind) || failureReason(job.failure.detail)}</span> : null}
    {retry ? <ActionButton variant="ghost" className="-ms-1 h-8 px-2 text-meta max-md:h-11" action={retry}><RotateCcw aria-hidden="true" />再发一次</ActionButton> : null}
  </div>
}
