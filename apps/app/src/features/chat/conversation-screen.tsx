import { useEffect, useRef, useState } from 'react'
import { Link, useNavigate, useRouterState } from '@tanstack/react-router'
import { ArrowDown, ArrowRightLeft, Bot, Check, ChevronRight, CircleCheck, Clock, CircleQuestionMark, CircleStop, CircleX, Crown, Hourglass, Laptop, Link2, ListChecks, ListPlus, LoaderCircle, MessageSquareOff, MessageSquareReply, Plus, RotateCcw, RotateCw, Send, SlidersHorizontal, Unplug, WifiOff } from 'lucide-react'
import { ActionCard, Details, EmptyState, InlineError, PageActions, PrimaryMark } from '@/components/app/kit'
import { BotFace, botMood, botTint, useDark } from '@/components/app/bot-face'
import { faceOf } from '@vibeface/client/avatar/face.ts'
import { Button } from '@/components/ui/button'
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { failureText, outcomeUnknown, taskError, workFailure } from '@vibeface/client/lib/errors.ts'
import { terms } from '@/lib/glossary'
import { cn } from '@/lib/utils'
import { ActionButton } from '@/features/workspace/present'
import { ChatSkeleton, useCreateBot } from '@/features/workspace/workspace-shell'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { QuestionCard } from '@/features/workspace/question-card'
import { chatTaskState, nodeName, progressLabel, queueHoldLabels, staleTaskLabel, taskActive, taskAgent, taskCopy, taskLabel, taskProgress, taskStale } from '@vibeface/client/workspace/labels.ts'
import type { BotChangeRecord, ManagerJob, SentBotMessage, WorkRecord } from '@vibeface/client/workspace/types.ts'
import { botCopy, placeChanges, scheduleName, scheduleTrigger } from '@vibeface/client/workspace/bot-settings.ts'
import { botReadiness, pendingChecks } from '@vibeface/client/workspace/model.ts'
import { awaitsAnswer, foldAnswers, workApproval } from '@vibeface/client/workspace/questions.ts'
import { providerIcons } from '@/features/devices/ai-settings'
import { providerNames, type ProviderId } from '@vibeface/client/lib/node.ts'
import { ChangeCard } from './change-card'
import { Chat, type ChatEntry } from './elements/chat'
import type { ChatMessage } from './elements/message'
import { PromptInput } from './elements/prompt-input'
import { DropdownMenu } from 'radix-ui'
import { InputQueue, WorkspaceInputStatus } from '@/features/workspace/workspace-chat'
import type { InputIntent, InputTarget } from '@vibeface/client/workspace/types.ts'
import { awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from '@vibeface/client/chat/protocol.ts'
import { setUpOnManager } from '@vibeface/client/identity/copy.ts'
import { chatTimeline, firstUnread, taskMessage } from '@vibeface/client/chat/timeline.ts'
import { networkKey, storageOf } from '@vibeface/client/workspace/storage.ts'
import { webPlatform } from '@/lib/platform'
import './chat.css'

const busyStates = ['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling']
// The translated cause alone: an unknown one reads as its own first line, not the generic sentence above it.
const failureReason = (raw: string) => taskError(raw).split('\n').at(-1) ?? ''
// The owner wrote the reply but it has not synced here yet; its streamed text stands in until it lands.
const streamingStates = [...busyStates, 'publishing']
type ComposerDraft = { text: string; files: File[]; intent: InputIntent; target: string; conversationId: string }

function NoBot() {
  const { bots } = useWorkspace()
  const create = useCreateBot()
  return bots.length
    ? <EmptyState icon={Bot} title={botCopy.notFound} className="my-auto" action={<Button asChild variant="outline"><Link to="/">查看 Bot</Link></Button>} />
    : <EmptyState icon={Bot} title="还没有 Bot" className="my-auto" action={<Button type="button" onClick={create}><Plus aria-hidden="true" />新 Bot</Button>} />
}

/** `missing`: the route names a bot this network does not have. */
export function ConversationScreen({ missing = false, active }: { missing?: boolean; active: boolean }) {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const routeRoutine = useRouterState({ select: (state) => 'routine' in state.location.search && typeof state.location.search.routine === 'string' ? state.location.search.routine : null })
  const { directory, ready, retryNow } = useDevice()
  const draftStorage = storageOf(webPlatform.storage)
  const draftKey = networkKey('bot-drafts', ready?.principal.network_id ?? '')
  const [drafts, setDrafts] = useState<Map<string, string>>(() => { const stored = draftStorage.readStorage<unknown>(draftKey, {}); return stored && typeof stored === 'object' && !Array.isArray(stored) ? new Map(Object.entries(stored).filter((entry): entry is [string, string] => typeof entry[1] === 'string')) : new Map() })
  const [sendError, setSendError] = useState<unknown>(null)
  const [files, setFiles] = useState<File[]>([])
  const [intent, setIntent] = useState<InputIntent>('continue')
  const [selectedTarget, setSelectedTarget] = useState(routeRoutine ? `routine:${routeRoutine}` : 'main')
  const { conversationId, data, chatSlot } = workspace
  const pendingMessages = workspace.pendingMessages.filter((item) => item.conversation_id === conversationId && item.target.kind === 'bot_main')
  const bot = workspace.selectedBot
  const dark = useDark()
  const tint = bot ? botTint(faceOf(bot.bot_id, bot.avatar).hue, dark) : undefined
  const text = drafts.get(conversationId) ?? ''
  const draftRef = useRef<ComposerDraft>({ text, files, intent, target: selectedTarget, conversationId })
  if (draftRef.current.text !== text || draftRef.current.files !== files || draftRef.current.intent !== intent || draftRef.current.target !== selectedTarget || draftRef.current.conversationId !== conversationId) draftRef.current = { text, files, intent, target: selectedTarget, conversationId }
  const draft = draftRef.current
  const savingDrafts = useRef(new Set<ComposerDraft>())
  const jobs = data?.scheduler?.jobs ?? []
  const unanswered = data?.scheduler?.unanswered ?? []
  const activeJobs = jobs.filter((job) => busyStates.includes(job.state))
  const checks = pendingChecks(data, workspace.nodes, conversationId)
  const live = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (!streamingStates.includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id))))
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const reach = botReach(workspace.nodes), agentReady = botCanSend(reach)
  const sendDisabled = !workspace.canSaveMessages || !bot || !canSend({ ...gate, busy: false })
  const botBusy = activeJobs.length > 0
  const inputTarget: InputTarget | null = bot ? selectedTarget.startsWith('routine:') ? { kind: 'routine', bot_id: bot.bot_id, trigger: selectedTarget.slice(8) } : { kind: 'bot_main', bot_id: bot.bot_id } : null
  const targetBusy = inputTarget?.kind === 'routine' ? Boolean(data?.scopeJobs.some((job) => job.trigger === inputTarget.trigger && busyStates.includes(job.state))) : botBusy
  const stopJob = jobs.find((job) => data?.scheduler?.capabilities?.cancel === true && ['queued', 'steering', 'claimed', 'running'].includes(job.state))
  const lookup = { nodes: workspace.nodes, directory }

  const pendingRow = (message: ChatMessage, retry?: () => Promise<void>): ChatEntry => ({ key: message.message_id, anchor: message.role === 'user', message, retry })
  const interruptedRow = (reply: (typeof workspace.interrupted)[number]): ChatMessage => ({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })

  // Tasks sit right after the message that started them.
  const works = data?.works ?? []
  // A running turn paused on an approval reads as waiting; a steered input names the task it joined.
  const currentWorks = new Set(workspace.openWorks?.filter((work) => work.conversation_id === conversationId).map((work) => work.work_id))
  const confirming = new Set(checks.flatMap((check) => check.kind === 'work' ? [check.work_id] : []))
  const tasks: TaskRow[] = works.map((work, index) => {
    const shown = chatTaskState(work, works, workspace.nodes)
    const asking = workApproval({ work, conversationId, questions: data?.questions ?? [], answered: workspace.answered })
    const state = shown === 'running' && asking ? 'waiting' : shown
    const joined = work.turn_id ? works.slice(0, index).find((item) => item.thread_id === work.thread_id && item.turn_id === work.turn_id) : undefined
    const attention = state === 'waiting' || confirming.has(work.work_id) || currentWorks.has(work.work_id) && (taskStale(state, workspace.connectionOnline) || ['failed', 'rejected', 'dispatch_rejected', 'dispatch_unverified', 'unknown', 'unverified', 'awaiting_confirmation'].includes(state))
    return { id: work.work_id, state, attention, device: work.target_node, summary: taskProgress(work, workspace.liveProgress), agent: taskAgent(work), trigger: work.trigger, work, joined: joined?.title }
  })
  const groups = new Map<string, TaskRow[]>()
  for (const task of tasks) groups.set(task.trigger, [...groups.get(task.trigger) ?? [], task])

  // What a turn changed in the bot shows after its reply, or after the message while no reply exists.
  const changes = data?.changes ?? []
  const placedChanges = placeChanges(data?.messages ?? [], changes)
  const changeRow = (records: BotChangeRecord[]) => ({ key: `change:${records[0].change_id}`, node: <ChangeCard records={records} schedules={data?.bot_settings?.schedules ?? []} overwritten={data?.overwritten ?? []} /> })

  // Questions and messages to other bots sit where they happened as rows, not bubbles; an answer shows inside its question.
  const questions = data?.questions ?? []
  const folded = foldAnswers(data?.messages ?? [], questions)
  const visible = (data?.messages ?? []).filter((message) => !folded.hidden.has(message.message_id))
  const taskGroups = new Map<string, ChatEntry[]>()
  for (const [trigger, group] of groups) {
    const messageId = taskMessage({ trigger, jobs, messages: visible, works })
    if (!messageId || !bot) continue
    const cards = group.map((task) => <TaskCard key={task.id} task={task} botId={bot.bot_id} device={nodeName(task.device, lookup)} here={task.device === ready?.entry.device_id} live={workspace.connectionOnline} away={task.state === 'dispatch_waiting' && workspace.nodes.some((node) => node.device_id === task.device && !node.online)} />)
    const working = group.filter((task) => workspace.connectionOnline && (taskActive(task.state) || task.state === 'awaiting_children') && !task.attention).length
    const attention = group.filter((task) => task.attention).length
    const label = [`${group.length} 个任务`, working ? `${working} 进行中` : '', attention ? `${attention} 待处理` : '', !working && !attention && group.every((task) => task.state === 'completed') ? '已完成' : ''].filter(Boolean).join(' · ')
    const row = { key: `task-group:${trigger}`, node: group.length > 1 ? <Details label={label} className="min-w-0">{cards}</Details> : cards[0] }
    taskGroups.set(messageId, [...taskGroups.get(messageId) ?? [], row])
  }
  const merged = new Map(jobs.filter((job) => job.state === 'steered').map((job) => [job.message_id, job.outcome]))
  const timeline = chatTimeline(data?.messages ?? [], [
    ...questions.map((question) => ({ key: `question:${question.question_id}`, created_at_ms: question.created_at_ms, waiting: awaitsAnswer(question, workspace.answered, folded.answers), node: <div id={`question-${question.question_id}`} className="scroll-mb-4"><QuestionCard question={question} answer={folded.answers.get(question.question_id)} /></div> })),
    ...(data?.sent ?? []).map((sent) => ({ key: `sent:${sent.message_id}`, created_at_ms: sent.created_at_ms, node: <SentToBot sent={sent} to={workspace.bots.find((bot) => bot.conversation_id === sent.to) ?? { name: '另一个 Bot' }} /> })),
  ], merged)

  // 重试 sits on the newest reply when it failed or was stopped; it reads the conversation again first and words what it sends by what the reply may have done.
  const newest = data?.messages.at(-1)
  const endedAsk = newest && ['failed', 'stopped', 'interrupted'].includes(newest.status ?? '') ? newest.in_reply_to : undefined
  // A reply whose outcome is unknown may have done its work: it is checked, never retried.
  const retryOf = (messageId: string | undefined) => messageId && !sendDisabled && !jobs.some((job) => job.message_id === messageId && outcomeUnknown(job.failure?.kind, job.failure?.detail))
    ? async () => { setSendError(null); await workspace.retryReply(messageId) } : undefined
  const retry = retryOf(endedAsk)
  const replied = new Set((data?.messages ?? []).map((message) => message.message_id))
  // A message sent while the bot replies waits behind that reply, so it shows after it until its own turn starts.
  const replying = jobs.some((job) => ['claimed', 'steering', 'running', 'cancelling'].includes(job.state))
  const queued = new Set(replying ? jobs.filter((job) => job.message_id && ['pending_delivery', 'queued', 'steering'].includes(job.state)).map((job) => job.message_id) : [])
  const merging = new Set(jobs.filter((job) => job.state === 'steering').map((job) => job.message_id))
  const entries: ChatEntry[] = placedChanges.first.map(changeRow)
  const waiting: ChatEntry[] = []
  for (const message of timeline.messages) {
    const start = entries.length
    for (const aside of timeline.before.get(message.message_id) ?? []) entries.push(aside)
    const schedule = message.role === 'assistant' ? scheduleTrigger(message.in_reply_to) : null
    const detail = jobs.find((job) => job.reply_message_id === message.message_id)?.failure?.detail
    if (schedule) {
      // A schedule run has no message of the person; its reply stands alone and the tasks it started follow it.
      entries.push({ key: message.message_id, message, schedule: scheduleName(schedule, data?.bot_settings, changes), detail })
    } else if (!folded.hidden.has(message.message_id)) entries.push({ ...pendingRow(merged.has(message.message_id) ? { ...message, state: 'merged', merged: merged.get(message.message_id) } : message, message.message_id === newest?.message_id ? retry : undefined), detail })
    entries.push(...taskGroups.get(message.message_id) ?? [])
    const recordedJob = jobs.find((job) => job.reply_message_id === message.message_id || !job.reply_message_id && job.message_id === message.message_id)
    if (recordedJob?.runtime) entries.push({ key: `runtime:${recordedJob.trigger}`, node: <Details label="运行信息">{runtimeLabel(recordedJob.runtime)}</Details> })
    if (message.status === 'failed' && !recordedJob?.pending_check && (!recordedJob || !canRetryJob(recordedJob))) entries.push({ key: `query:${message.message_id}`, node: <ActionButton variant="ghost" action={() => workspace.refresh()}>再查一次</ActionButton> })
    if (recordedJob?.state === 'reconciling' || recordedJob?.pending_check) entries.push({ key: `reconcile:${recordedJob.trigger}`, node: <ActionCard icon={CircleQuestionMark} title="这次回复的结果待确认" actions={<ActionButton action={() => workspace.refresh()}>再查一次</ActionButton>}><p className="text-meta text-muted-foreground">设备正在查询上次回复，系统不会自动重发。</p></ActionCard> })
    for (const job of jobs) {
      if (job.message_id !== message.message_id) continue
      if (job.state === 'queued' && message.role === 'user') entries.push({ key: `queued-actions:${job.trigger}`, node: <p className="text-meta text-muted-foreground">{job.hold_reason ? queueHoldLabels[job.hold_reason] : '等待开始工作'}</p> })
      if ((job.state !== 'failed' && job.state !== 'rejected') || replied.has(job.reply_message_id ?? '')) continue
      // A reply that ended with no message of its own: 重试 sits on this line while the message is the newest.
      const again = message.role === 'user' && message.message_id === newest?.message_id ? retryOf(message.message_id) : undefined
      entries.push({ key: `job:${job.trigger}`, node: <JobLine job={job} retry={again} refresh={() => workspace.refresh()} /> })
    }
    for (const reply of workspace.interrupted) if (reply.after === message.message_id) entries.push(pendingRow(interruptedRow(reply), message.message_id === newest?.message_id ? retryOf(message.message_id) : undefined))
    for (const turn of placedChanges.after.get(message.message_id) ?? []) entries.push(changeRow(turn))
    if (queued.has(message.message_id)) waiting.push(...entries.splice(start).map((entry) => 'message' in entry && entry.message.message_id === message.message_id ? { ...entry, message: { ...entry.message, state: merging.has(message.message_id) ? 'merging' as const : 'queued' as const } } : entry))
  }
  entries.push(...timeline.rest)
  for (const reply of workspace.interrupted) if (!reply.after || !data?.messages.some((message) => message.message_id === reply.after)) entries.push(pendingRow(interruptedRow(reply)))
  for (const pending of pendingMessages) {
    if (!data?.messages.some((message) => message.message_id === pending.message_id)) entries.push(pendingRow({ message_id: pending.message_id, event_hash: pending.message_id, role: 'user', text: pending.text, created_at_ms: pending.created_at_ms, state: 'sending' }))
    entries.push({ key: `delivery:${pending.message_id}`, node: <WorkspaceInputStatus pending={pending} /> })
  }
  for (const progress of live) entries.push(pendingRow({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0, state: 'streaming' }))

  const owner = data?.conversation?.owner
  const here = workspace.nodes.find((node) => node.device_id === data?.local_node)
  const ownerAway = Boolean(owner && here && (botReadiness(here) === 'ready' || botReadiness(here) === 'unverified') && owner !== here.device_id && !workspace.nodes.some((node) => node.device_id === owner && node.online && !node.revoked))
  const lastUnanswered = data?.messages.filter((message) => unanswered.includes(message.message_id)).at(-1)
  const activeReply = activeJobs.find((job) => job.state === 'cancelling') ?? activeJobs.find((job) => job.state === 'running') ?? activeJobs.find((job) => job.state === 'claimed') ?? activeJobs[0]
  const status = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.trigger === activeReply?.trigger && item.status)?.status
  // The owner holds the reply's state; when it does not answer, a waiting message shows that state as unknown.
  const remote = data?.scheduler?.remote
  if (remote && !remote.reachable && !ownerAway && newest?.role === 'user') entries.push({ key: 'owner-unknown', node: <p role="status" className="flex min-h-7 items-center gap-2 text-meta text-muted-foreground"><Unplug aria-hidden="true" className="size-4 shrink-0" />{nodeName(remote.owner, lookup)} 连不上，回复情况未知</p> })
  // A question waiting on the person is the status already; a spinner under it would say the bot is still thinking.
  const toAnswer = questions.filter((question) => awaitsAnswer(question, workspace.answered, folded.answers))
  const asked = toAnswer[0]
  const stopping = activeJobs.some((job) => job.state === 'cancelling')
  const activity = !workspace.connectionOnline || remote?.reachable === false ? '暂时收不到最新进展，显示最近记录'
    : stopping ? '正在确认停止回复'
    : activeReply?.state === 'pending_delivery' ? '等待设备接收'
    : activeReply?.state === 'queued' ? '等待处理'
    : activeReply?.state === 'claimed' ? '正在准备'
    : activeReply?.state === 'steering' ? '正在加入本轮'
    : status ? progressLabel(status) : '正在处理你的要求'
  if (botBusy && !live.length && !asked) entries.push({ key: 'activity', node: <p role="status" className="flex min-h-7 items-center gap-2 text-meta text-muted-foreground">{stopping ? <CircleStop aria-hidden="true" className="size-4 shrink-0" />
    : bot ? <BotFace bot={bot} mood={workspace.connectionOnline && remote?.reachable !== false ? 'thinking' : 'asleep'} size={24} /> : null}{activity}</p> })
  entries.push(...waiting)
  // The replies and questions this chat's badge counted when it came on screen start under 新消息.
  const unread = firstUnread(timeline, workspace.fresh, (aside) => 'waiting' in aside && aside.waiting === true && workspace.fresh.has(aside.key))
  const unreadAt = unread ? entries.findIndex((entry) => entry.key === ('key' in unread ? unread.key : unread.message_id)) : -1
  if (unreadAt >= 0) entries.splice(unreadAt, 0, { key: 'unread', node: <p className="flex items-center gap-3 text-meta text-primary before:h-px before:flex-1 before:bg-primary/40 after:h-px after:flex-1 after:bg-primary/40">新消息</p> })

  // Everything waiting on the person ends the conversation, so it scrolls with it instead of covering it.
  const cards = [
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

  const notice = !workspace.connectionOnline ? ''
    : workspace.readOnlyNotice ? workspace.readOnlyNotice
    : workspace.writeNotice ? '同步中，稍后可发送' : ''
  // Questions sit where they were asked, so an older one scrolls away; the pill above the composer brings back the first still open.
  const waitingTarget = asked ? `question-${asked.question_id}` : cards.length ? 'chat-waiting' : null
  const waitingCount = toAnswer.length + cards.length
  const targetHidden = useOffscreen(waitingTarget, entries.length)
  const reason = composerHint({ ...gate, bot: reach })
  const needsSetup = Boolean(bot && !agentReady)
  const setupDevice = reachDevice(workspace.nodes, reach) ?? ready?.entry.device_id
  const away = reach === 'offline'
  const setupTo = setupDevice ? '/devices/$deviceId' : '/devices'
  const canSetUp = ready?.principal.can_manage === true || ready?.principal.source === 'local_session'

  // Picked files belong to the bot they were picked for.
  useEffect(() => { setSendError(null); setFiles([]); setSelectedTarget(routeRoutine ? `routine:${routeRoutine}` : 'main'); setIntent('continue') }, [conversationId, routeRoutine])
  // Nothing pushes a device's bot readiness, so while a device that could reply to an open bot is not ready, read it again every 10s, or every 2s while it starts.
  const latest = useRef(workspace)
  latest.current = workspace
  const awaiting = Boolean(bot) && awaitsDevice(reach), starting = reach === 'starting'
  useEffect(() => {
    if (!awaiting) return
    // A read still running is left to finish, so a slow one never queues a follow-up that reports its failure.
    const read = () => { if (latest.current.isCurrent() && !latest.current.refreshing) void latest.current.refresh().catch(() => {}) }
    read()
    const timer = setInterval(read, starting ? 2000 : 10000)
    return () => clearInterval(timer)
  }, [awaiting, starting])

  function clearSentDraft(id: string, sent: string) {
    setDrafts((current) => {
      if (current.get(id)?.trim() !== sent.trim()) return current
      const next = new Map(current)
      next.delete(id)
      draftStorage.writeStorage(draftKey, Object.fromEntries(next))
      return next
    })
  }
  const setText = (value: string) => setDrafts((current) => { const next = new Map(current).set(conversationId, value); draftStorage.writeStorage(draftKey, Object.fromEntries(next)); return next })

  if (missing) return <section className="conversation-screen" aria-label="对话"><Chat key="missing" entries={[]} empty={<NoBot />} /></section>
  const history = active && bot ? <TaskHistory key={`task-history:${conversationId}`} tasks={tasks} botId={bot.bot_id} name={bot.name} /> : null
  if (bot && !data) return <section className="conversation-screen" aria-label="对话">{history}<ChatSkeleton /></section>

  return (
    <section className="conversation-screen" aria-label="对话">
      {history}
      <Chat
        key={conversationId}
        entries={entries}
        empty={bot ? <div className="mx-auto my-auto grid max-w-md justify-items-center gap-4 px-4 py-12 text-center">
          <BotFace bot={bot} mood={botMood(bot, workspace)} size={96} halo={tint?.glow} />
          <div className="grid justify-items-center gap-1">
            <h2 className="text-heading font-semibold">{bot.name}</h2>
            {workspace.primary === bot.conversation_id ? <PrimaryMark labeled /> : null}
          </div>
          {!needsSetup ? <p className="text-sm leading-relaxed text-muted-foreground">说说你想完成的事，也可以随时补充要求。</p> : null}
          {starting ? <p role="status" className="flex items-center gap-1.5 text-muted-foreground"><LoaderCircle aria-hidden="true" className={`size-4 shrink-0 ${workspace.connectionOnline ? 'animate-spin' : ''}`} />Bot 正在启动</p>
            : needsSetup ? <>
            <p className="flex items-center gap-1.5 text-muted-foreground"><MessageSquareOff aria-hidden="true" className="size-4 shrink-0" />Bot 还不能回复</p>
            {away || canSetUp ? <Button asChild><Link to={setupTo} params={{ deviceId: setupDevice ?? '' }} search={{ bot: bot.bot_id }}><Bot aria-hidden="true" />{away ? '查看设备' : '设置运行 Bot 的设备'}</Link></Button>
              : <p className="flex items-center gap-1.5 text-meta text-muted-foreground"><Crown aria-hidden="true" className="size-4 shrink-0" />{setUpOnManager((directory?.members ?? []).filter((member) => member.manage).map((member) => member.name))}</p>}
          </>
            : <div className="flex flex-wrap justify-center gap-2">
              {starters[bot.project ? 'project' : 'general'].map((example) => <Button key={example} type="button" variant="outline" className="rounded-full text-muted-foreground hover:text-foreground" style={{ borderColor: tint?.edge }} onClick={() => { setText(example); document.getElementById('message-text')?.focus() }}>{example}</Button>)}
            </div>}
        </div> : workspace.synced ? <NoBot /> : null}
      />
      {bot && inputTarget ? <PromptInput
        id="message-text"
        above={<div className="flex max-w-full flex-col items-start gap-2">
          {!workspace.connectionOnline ? <p role="status" className="flex min-h-8 max-w-full flex-wrap items-center gap-1.5 rounded-xl border bg-card ps-2.5 pe-1 text-meta font-medium max-md:min-h-11">
          <WifiOff aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="min-w-0 break-words">连不上{ready ? nodeName(ready.entry.device_id, lookup) : '设备'}</span>
          <Button type="button" variant="ghost" size="sm" className="h-7 shrink-0 rounded-full max-md:h-9" onClick={retryNow}><RotateCw aria-hidden="true" />重试连接</Button>
        </p> : null}
        {waitingTarget && (targetHidden || !workspace.connectionOnline) ? <button type="button" className="flex min-h-8 max-w-full items-center gap-1.5 rounded-full border border-warning-border bg-card ps-2.5 pe-3 text-meta font-medium transition-colors hover:bg-warning-soft active:bg-warning-soft animate-in fade-in-0 slide-in-from-bottom-1 duration-200 ease-out motion-reduce:animate-none max-md:min-h-11 max-md:ps-3"
          onClick={() => document.getElementById(waitingTarget)?.scrollIntoView({ block: asked ? 'center' : 'end', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' })}>
          <terms.inbox.icon aria-hidden="true" className="size-4 shrink-0 text-warning" /><span className="min-w-0 break-words">{!workspace.connectionOnline ? '最近记录 · ' : ''}{waitingCount > 1 ? `${waitingCount} 项${terms.inbox.label}` : terms.inbox.label}</span><ArrowDown aria-hidden="true" className="size-3.5 shrink-0 text-muted-foreground" />
        </button> : null}
        </div>}
        tint={tint}
        value={text}
        hint={notice}
        reason={reason}
        disabled={sendDisabled}
        busy={false}
        sendLabel={intent === 'defer' ? '保存为稍后处理' : intent === 'new_task' ? '发送为新任务' : targetBusy ? '补充当前工作' : '发送'}
        tools={<IntentMenu intent={intent} busy={targetBusy} onChange={setIntent} />}
        onStop={workspace.writable && (inputTarget.kind === 'bot_main' ? stopJob : targetBusy) ? () => { setSendError(null); workspace.stopInputTarget({ target: inputTarget }).catch(setSendError) } : undefined}
        onChange={setText}
        files={files}
        onFiles={setFiles}
        onSubmit={() => {
          if (sendDisabled || !text.trim() && !files.length) return
          const submittedDraft = draft
          if (savingDrafts.current.has(submittedDraft)) return
          savingDrafts.current.add(submittedDraft)
          setSendError(null)
          const submittedFiles = files
          workspace.sendMessage(text, submittedFiles, undefined, { target: inputTarget, intent, onSaved: (input) => { if (!workspace.isCurrent() || latest.current.conversationId !== conversationId || latest.current.view !== "chat" || draftRef.current !== submittedDraft) return; if (intent === "new_task" && (input.target.kind === "task" || input.target.kind === "channel_task")) void navigate({ to: "/tasks/$taskId", params: { taskId: input.target.task_id } }) } }).then((sent) => { if (sent && workspace.isCurrent()) { if (latest.current.conversationId !== conversationId || draftRef.current === submittedDraft) clearSentDraft(conversationId, text); if (draftRef.current === submittedDraft) setFiles((current) => current === submittedFiles ? [] : current) } }).catch(setSendError).finally(() => { savingDrafts.current.delete(submittedDraft) })
        }}
      >
        <InlineError error={sendError} className="basis-full" />
        {inputTarget.kind === 'routine' ? <p className="flex basis-full items-center gap-2"><span className="min-w-0 flex-1 truncate">{targetBusy ? '补充给' : '发送给'} · 例行任务 · {scheduleName(inputTarget.trigger, data?.bot_settings, changes)}</span><Button type="button" variant="ghost" size="sm" className="max-md:min-h-11" onClick={() => setSelectedTarget('main')}>改回主聊天</Button></p> : null}
        <InputQueue target={inputTarget} />
        {needsSetup && !starting && entries.length && !notice ? <Link to={setupTo} params={{ deviceId: setupDevice ?? '' }} search={{ bot: bot?.bot_id }} className="inline-flex items-center gap-0.5 font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11">去设置<ChevronRight aria-hidden="true" className="size-3.5" /></Link> : null}
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

type TaskRow = { id: string; state: string; attention: boolean; device: string; summary: string; agent: ProviderId | null; trigger: string; work: WorkRecord; joined?: string }

function TaskIcon({ state, stale, away = false }: { state: string; stale: boolean; away?: boolean }) {
  const failed = ['failed', 'rejected', 'dispatch_rejected'].includes(state)
  const Icon = stale ? CircleQuestionMark : away ? WifiOff : state === 'completed' ? CircleCheck
    : failed ? CircleX : state === 'interrupted' ? CircleStop
    : state === 'waiting' ? terms.approval.icon : state === 'awaiting_children' ? Hourglass
    : taskActive(state) ? LoaderCircle : CircleQuestionMark
  return <Icon aria-hidden="true" className={`size-4 shrink-0 ${state === 'completed' ? 'text-success' : failed ? 'text-danger' : state === 'waiting' ? 'text-warning' : !stale && !away && taskActive(state) ? 'animate-spin text-info motion-reduce:animate-none' : 'text-muted-foreground'}`} />
}

function TaskCard({ task, botId, device, here, live, away }: { task: TaskRow; botId: string; device: string; here: boolean; live: boolean; away: boolean }) {
  const failed = task.state === 'failed' || task.state === 'rejected' || task.state === 'dispatch_rejected'
  const title = task.work.title || '任务'
  const ongoing = taskActive(task.state) || task.state === 'awaiting_children'
  const stale = taskStale(task.state, live) || !live && task.state === 'awaiting_children'
  const label = stale ? staleTaskLabel : away ? taskCopy.away : taskLabel(task.state)
  const reason = !ongoing && task.state !== 'completed' ? workFailure(task.work).split('\n')[0] : ''
  return <div className="@container"><Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} aria-label={`${title} · ${label} · 查看任务`} className="-mx-2 flex min-h-9 items-start gap-2 rounded-lg px-2 py-2 text-meta transition-colors hover:bg-muted/60 hover:text-foreground active:bg-muted max-md:min-h-11 max-md:py-3">
    <TaskIcon state={task.state} stale={stale} away={away} />
    <span className="min-w-0 flex-1">{ongoing ? <><span className="flex min-w-0 flex-col @sm:flex-row @sm:items-baseline @sm:gap-1"><span className="truncate font-medium">{title}</span><TaskMeta device={device} here={here} label={label} agent={task.agent} className="text-meta" /></span>
      {task.joined ? <span className="flex min-w-0 items-center gap-1 text-meta text-muted-foreground"><Link2 aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">补充到「{task.joined}」</span></span>
        : taskActive(task.state) && task.summary ? <span className="line-clamp-2 whitespace-pre-wrap break-words text-meta text-muted-foreground">{stale ? '最近进展：' : ''}{task.summary}</span> : null}</>
      : <><span className="text-muted-foreground">{label} · 查看任务</span>{reason ? <span className={`block break-words ${failed ? 'text-danger' : 'text-muted-foreground'}`}>{reason}</span> : null}</>}</span>
    {task.state === 'completed' ? null : <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />}
  </Link></div>
}

function TaskHistory({ tasks, botId, name }: { tasks: TaskRow[]; botId: string; name: string }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const [open, setOpen] = useState(false)
  const leaving = useRef(false)
  useEffect(() => () => { leaving.current = true }, [])
  const working = tasks.filter((task) => workspace.connectionOnline && !task.attention && (taskActive(task.state) || task.state === 'awaiting_children')).length
  const attention = tasks.filter((task) => task.attention).length
  const count = [working ? `${working} 进行中` : '', attention ? `${attention} 待处理` : ''].filter(Boolean).join(' · ')
  return <>
    <PageActions><Button type="button" variant="ghost" size="sm" aria-label={`任务${count ? ` · ${count}` : ''}`} onClick={() => { leaving.current = false; setOpen(true) }}><ListChecks aria-hidden="true" /><span>任务</span>{count ? <span className="text-meta text-muted-foreground max-sm:sr-only">{count}</span> : null}{count ? <span className="text-meta sm:hidden">{working + attention}</span> : null}</Button></PageActions>
    <Sheet open={open} onOpenChange={setOpen}>
      <SheetContent className="!w-full min-w-0 gap-0 sm:!max-w-md" onCloseAutoFocus={(event) => { if (leaving.current) event.preventDefault() }}>
        <SheetHeader className="shrink-0 pe-14"><SheetTitle>任务记录</SheetTitle><SheetDescription>{name} 的任务</SheetDescription></SheetHeader>
        <div className="min-h-0 flex-1 overflow-y-auto px-4 pb-6">
          {!workspace.connectionOnline || workspace.incomplete ? <p role="status" className="pb-3 text-meta text-muted-foreground">{!workspace.connectionOnline ? '暂时收不到最新任务状态，显示最近记录。' : '任务记录正在同步，显示已读取的部分。'}</p> : null}
          {!workspace.data ? <p role="status" className="py-6 text-meta text-muted-foreground">正在读取任务记录</p> : tasks.length ? <ul className="divide-y">{tasks.map((task) => {
            const stale = taskStale(task.state, workspace.connectionOnline) || !workspace.connectionOnline && task.state === 'awaiting_children'
            const away = task.state === 'dispatch_waiting' && workspace.nodes.some((node) => node.device_id === task.device && !node.online)
            return <li key={task.id}><Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} onClick={() => { leaving.current = true; setOpen(false) }} className="-mx-2 flex min-h-11 min-w-0 items-start gap-2 rounded-lg px-2 py-3 hover:bg-muted/60 active:bg-muted">
              <TaskIcon state={task.state} stale={stale} away={away} />
              <span className="grid min-w-0 flex-1 gap-1"><span className="break-words font-medium">{task.work.title || '任务'}</span><span className="flex min-w-0 flex-wrap gap-x-1 text-meta text-muted-foreground"><span>{stale ? staleTaskLabel : away ? taskCopy.away : taskLabel(task.state)}</span><span aria-hidden="true">·</span><span className="min-w-0 break-words">{nodeName(task.device, { nodes: workspace.nodes, directory })}</span></span></span>
              <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            </Link></li>
          })}</ul> : <p className="py-6 text-meta text-muted-foreground">{!workspace.connectionOnline || workspace.incomplete ? '最近记录中还没有任务。' : '还没有任务'}</p>}
        </div>
      </SheetContent>
    </Sheet>
  </>
}

/** Device and state never yield to a long title: in a column narrower than `@sm` they take their own line. */
function TaskMeta({ device, here, label, agent, className = '' }: { device: string; here: boolean; label: string; agent: ProviderId | null; className?: string }) {
  const by = agent ? providerNames[agent] : ''
  const Agent = agent ? providerIcons[agent] : null
  const dot = <span aria-hidden="true">·</span>
  return <span className={`flex min-w-0 shrink-0 items-center gap-1 text-muted-foreground ${className}`}>
    <span aria-hidden="true" className="hidden @sm:inline">·</span>
    {Agent ? <><Agent aria-hidden="true" className="size-3.5 shrink-0" /><span className="shrink-0">{by}</span>{dot}</> : null}
    {here ? <span className="sr-only">{device}</span> : <><Laptop aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">{device}</span>{dot}</>}
    <span className="shrink-0">{label}</span>
  </span>
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

/** `retry`: 重试, only offered while its message is the newest. */
function JobLine({ job, retry, refresh }: { job: ManagerJob; retry?: () => Promise<void>; refresh: () => Promise<void> }) {
  return <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta">
    <span className="flex shrink-0 items-center gap-1 text-danger"><CircleX aria-hidden="true" className="size-4 shrink-0" />Bot 没能回复</span>
    {job.failure ? <span className="min-w-0 truncate text-muted-foreground" title={job.failure.detail}>{failureText(job.failure.kind) || failureReason(job.failure.detail)}</span> : null}
    {retry ? <ActionButton variant="ghost" className="-ms-1 h-8 px-2 text-meta max-md:h-11" action={retry}><RotateCcw aria-hidden="true" />重试</ActionButton> : <ActionButton variant="ghost" action={refresh}>再查一次</ActionButton>}
  </div>
}

const intents = [
  { value: 'continue', label: '发送', icon: Send },
  { value: 'new_task', label: '新任务', icon: ListPlus },
  { value: 'defer', label: '稍后处理', icon: Clock },
] as const

/** How the next message goes, folded into one icon inside the box; the icon turns to the choice while it is not a plain send. */
function IntentMenu({ intent, busy, onChange }: { intent: InputIntent; busy: boolean; onChange: (intent: InputIntent) => void }) {
  const current = intents.find((item) => item.value === intent) ?? intents[0]
  const label = (item: (typeof intents)[number]) => item.value === 'continue' && busy ? '补充当前工作' : item.label
  return <DropdownMenu.Root>
    <DropdownMenu.Trigger asChild>
      <Button type="button" variant="ghost" size="icon-sm" className={cn('size-9 rounded-full max-md:size-11', intent === 'continue' ? 'text-muted-foreground hover:text-foreground' : 'bg-muted text-foreground')} aria-label={`发送方式：${label(current)}`} title="发送方式" onMouseDown={(event) => event.preventDefault()}>
        {intent === 'continue' ? <SlidersHorizontal aria-hidden="true" /> : <current.icon aria-hidden="true" />}
      </Button>
    </DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content side="top" align="start" sideOffset={6} className="z-50 grid min-w-40 gap-0.5 rounded-xl border bg-popover p-1 text-popover-foreground shadow-lg">
        <DropdownMenu.RadioGroup value={intent} onValueChange={(value) => { if (value === 'continue' || value === 'new_task' || value === 'defer') onChange(value) }}>
          {intents.map((item) => <DropdownMenu.RadioItem key={item.value} value={item.value} className="flex min-h-9 cursor-default items-center gap-2 rounded-lg px-2.5 text-sm outline-none select-none data-highlighted:bg-muted max-md:min-h-11">
            <item.icon aria-hidden="true" className="size-4 text-muted-foreground" />
            <span className="flex-1">{label(item)}</span>
            <DropdownMenu.ItemIndicator><Check aria-hidden="true" className="size-4" /></DropdownMenu.ItemIndicator>
          </DropdownMenu.RadioItem>)}
        </DropdownMenu.RadioGroup>
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
}
