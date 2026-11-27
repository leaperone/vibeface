import { BotFace,botMood } from '@/components/app/bot-face'
import { Elapsed } from "@/features/workspace/elapsed.tsx"
import { PromptInput } from '@/components/arc/conversation-composer'
import { Chat,type ChatEntry } from '@/components/arc/conversation-thread'
import { ActionCard,Details,Dot,EmptyState,InlineError,PageActions,PrimaryMark } from '@/components/arc/product-kit'
import { RoutedAction } from "@/components/arc/routed-action"
import { ActionButton } from '@/features/workspace/present'
import { useInbox } from '@/features/workspace/inbox'
import { QuestionCard } from '@/features/workspace/question-card'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { InputQueue,WorkspaceInputStatus } from '@/features/workspace/workspace-chat'
import { ChatSkeleton,ConnectionNotice,useCreateBot } from '@/features/workspace/workspace-shell'
import { terms } from '@/lib/glossary'
import { webPlatform } from '@/lib/platform'
import { Link,useNavigate,useRouterState } from '@tanstack/react-router'
import { awaitsDevice,botCanSend,botReach,canRetryJob,canSend,composerHint,inputIntents,inputIntentLabel,promptIntent,reachDevice,replyUnsettled,runtimeLabel,starterExamples } from '@vibeface/client/chat/protocol.ts'
import { useNotificationRecord } from '@vibeface/client/chat/use-notification-record.ts'
import { useHistoryLocation } from '@vibeface/client/chat/use-history-location.ts'
import { chatTimeline,firstUnread } from '@vibeface/client/chat/timeline.ts'
import { botRecap,recapGroups,recapPreview,routineState,type BotRecapItem } from '@vibeface/client/chat/companion-recap.ts'
import { cooperationExamples,useFirstCooperation } from '@vibeface/client/chat/first-cooperation.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { setUpOnManager } from '@vibeface/client/identity/copy.ts'
import { failureText,taskError } from '@vibeface/client/lib/errors.ts'
import { botCopy,placeChanges,scheduleName,scheduleTrigger } from '@vibeface/client/workspace/bot-settings.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { chatStatusLine,chatTaskState,nodeName,staleTaskLabel,taskActive,taskCopy,taskLabel,taskStale } from '@vibeface/client/workspace/labels.ts'
import { botReadiness,historySections,pendingChecks } from '@vibeface/client/workspace/model.ts'
import { awaitsAnswer,foldAnswers,questionParts,workApproval } from '@vibeface/client/workspace/questions.ts'
import { networkKey,storageOf } from '@vibeface/client/workspace/storage.ts'
import type { InputIntent,InputTarget,ManagerJob,PendingMessage,SentBotMessage,WorkRecord } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import buttonStyles from '@vibeface/uiarc/button/button.module.css'
import { Drawer,DrawerContent } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Popover,PopoverClose,PopoverContent,PopoverTrigger } from '@vibeface/uiarc/popover/popover.tsx'
import { ArrowDown,ArrowRightLeft,Bot,ChevronDown,ChevronRight,CircleCheck,CircleQuestionMark,CircleStop,CircleX,Crown,Hourglass,Laptop,ListChecks,LoaderCircle,MessageSquareOff,MessageSquareReply,Plus,RotateCcw,Send,Unplug,WifiOff } from 'lucide-react'
import { useEffect,useMemo,useRef,useState,type ReactNode,type RefObject } from 'react'
import { ChangeCard } from './change-card'
import './chat.css'
import type { ChatMessage } from './elements/message'
import { HistoryControls } from './history-controls'

const emptyList: never[] = []
const busyStates = ['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling']
// The translated cause alone: an unknown one reads as its own first line, not the generic sentence above it.
const failureReason = (raw: string) => taskError(raw).split('\n').at(-1) ?? ''
// The owner wrote the reply but it has not synced here yet; its streamed text stands in until it lands.
const streamingStates = [...busyStates, 'publishing']
type ComposerDraft = { text: string; files: File[]; target: string; intent: InputIntent | null; conversationId: string }

function NoBot() {
  const { bots } = useWorkspace()
  const create = useCreateBot()
  return bots.length
    ? <EmptyState icon={Bot} title={botCopy.notFound} className="my-auto" action={<RoutedAction variant="secondary" to="/">查看 Bot</RoutedAction>} />
    : <EmptyState icon={Bot} title="还没有 Bot" description="给你的 Bot 起个名字，接着交代你想完成的事。" className="my-auto" action={<Button type="button" onClick={create}><Plus aria-hidden="true" />新 Bot</Button>} />
}

/** `missing`: the route names a bot this network does not have. */
export function ConversationScreen({ missing = false, active }: { missing?: boolean; active: boolean }) {
  const workspace = useWorkspace()
  const inbox = useInbox()
  const generation = workspace.generation
  const navigate = useNavigate()
  const pathname = useRouterState({ select: (state) => state.location.pathname })
  const routeRoutine = useRouterState({ select: (state) => 'routine' in state.location.search && typeof state.location.search.routine === 'string' ? state.location.search.routine : null })
  const routeMessage = useRouterState({ select: (state) => 'message' in state.location.search && typeof state.location.search.message === 'string' ? state.location.search.message : null })
  const routeNotification = useRouterState({ select: (state) => 'notification' in state.location.search && typeof state.location.search.notification === 'string' ? state.location.search.notification : null })
  const { directory, ready } = useDevice()
  const draftStorage = storageOf(webPlatform.storage)
  const draftKey = networkKey('bot-drafts', ready?.principal.network_id ?? '')
  const [drafts, setDrafts] = useState<Map<string, string>>(() => { const stored = draftStorage.readStorage(draftKey, {}); return stored && typeof stored === 'object' && !Array.isArray(stored) ? new Map(Object.entries(stored).filter((entry): entry is [string, string] => typeof entry[1] === 'string')) : new Map() })
  const [sendError, setSendError] = useState<unknown>(null)
  const [files, setFiles] = useState<File[]>([])
  const [selectedMainTarget, setSelectedTarget] = useState('main')
  const selectedTarget = routeRoutine ? `routine:${routeRoutine}` : selectedMainTarget
  const [selectedIntent, setSelectedIntent] = useState<InputIntent | null>(null)
  const [focusRequest, setFocusRequest] = useState(0)
  const [activityOpen, setActivityOpen] = useState(false)
  // The control that opened the activity drawer gets focus back, or the header button once it is gone, unless a link in it navigated away.
  const activityFrom = useRef<HTMLElement | null>(null)
  const openActivity = (from: HTMLElement | null) => { activityFrom.current = from; setActivityOpen(true) }
  const { conversationId, data, chatSlot } = workspace
  const pendingMessages = workspace.pendingMessages.filter((item) => item.conversation_id === conversationId && item.target.kind === 'bot_main')
  const bot = workspace.selectedBot
  const text = drafts.get(conversationId) ?? ''
  const draftRef = useRef<ComposerDraft>({ text, files, target: selectedTarget, intent: selectedIntent, conversationId })
  if (draftRef.current.text !== text || draftRef.current.files !== files || draftRef.current.target !== selectedTarget || draftRef.current.intent !== selectedIntent || draftRef.current.conversationId !== conversationId) draftRef.current = { text, files, target: selectedTarget, intent: selectedIntent, conversationId }
  const draft = draftRef.current
  const savingDrafts = useRef(new Set<ComposerDraft>())
  const jobs = data?.scheduler?.jobs ?? emptyList
  const unanswered = data?.scheduler?.unanswered ?? []
  const activeJobs = jobs.filter((job) => busyStates.includes(job.state))
  const checks = pendingChecks(data, workspace.nodes, conversationId)
  const live = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (!streamingStates.includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id))))
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const reach = botReach(workspace.nodes), agentReady = botCanSend(reach)
  const sendDisabled = !workspace.canSaveMessages || !bot || !canSend({ ...gate, busy: false })
  const botBusy = activeJobs.length > 0
  const inputTarget: InputTarget | null = bot ? selectedTarget.startsWith('routine:') ? { kind: 'routine', bot_id: bot.bot_id, trigger: selectedTarget.slice(8) } : selectedTarget.startsWith('work:') ? { kind: 'work', work_id: selectedTarget.slice(5) } : { kind: 'bot_main', bot_id: bot.bot_id } : null
  const targetBusy = inputTarget?.kind === 'routine' ? Boolean(data?.scopeJobs.some((job) => job.trigger === inputTarget.trigger && busyStates.includes(job.state))) : inputTarget?.kind === 'work' ? Boolean(data?.works.some((work) => work.work_id === inputTarget.work_id && taskActive(work.state))) : botBusy
  const currentInput = data?.messages.find((message) => activeJobs.some((job) => job.message_id === message.message_id))
  const intent = selectedIntent ?? promptIntent(text)
  const targetLabel = inputTarget?.kind === 'routine' ? `例行任务 · ${scheduleName(inputTarget.trigger, data?.bot_settings, data?.changes)}` : inputTarget?.kind === 'work' ? data?.works.find((work) => work.work_id === inputTarget.work_id)?.title || '原工作' : intent === 'continue' && currentInput?.text ? `当前工作 · ${currentInput.text.slice(0, 48)}` : `${bot?.name ?? 'Bot'} · 主聊天`
  const targetAction = intent === 'continue' && targetBusy ? '补充给' : '发送给'
  const stopJob = jobs.find((job) => data?.scheduler?.capabilities?.cancel === true && ['queued', 'steering', 'claimed', 'running'].includes(job.state))
  const lookup = { nodes: workspace.nodes, directory }

  const pendingRow = (message: ChatMessage, retry?: () => Promise<void>): ChatEntry => ({ key: message.message_id, anchor: message.role === 'user', message, retry, conversationId })
  const interruptedRow = (reply: (typeof workspace.interrupted)[number]): ChatMessage => ({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: 0, state: 'interrupted' })

  const works = data?.works ?? []
  // A running turn paused on an approval reads as waiting; a steered input names the task it joined.
  const currentWorks = new Set(workspace.openWorks?.filter((work) => work.conversation_id === conversationId).map((work) => work.work_id))
  const confirming = new Set(checks.flatMap((check) => check.kind === 'work' ? [check.work_id] : check.kind === 'device' ? check.works.map((work) => work.work_id) : []))
  const tasks: TaskRow[] = works.map((work) => {
    const shown = chatTaskState(work, works, workspace.nodes)
    const asking = workApproval({ work, conversationId, questions: data?.questions ?? [], answered: workspace.answered })
    const state = shown === 'running' && asking ? 'waiting' : shown
    const attention = state === 'waiting' || confirming.has(work.work_id) || currentWorks.has(work.work_id) && (taskStale(state, workspace.connectionOnline) || ['failed', 'rejected', 'dispatch_rejected', 'dispatch_unverified', 'unknown', 'unverified', 'awaiting_confirmation'].includes(state))
    return { id: work.work_id, state, attention, device: work.target_node, work }
  })
  const runningTasks = tasks.filter((task) => workspace.connectionOnline && !task.attention && !(task.state === 'dispatch_waiting' && workspace.nodes.some((node) => node.device_id === task.device && !node.online)) && (taskActive(task.state) || task.state === 'awaiting_children')).length
  const selectedWork = inputTarget?.kind === 'work' ? tasks.find((task) => task.id === inputTarget.work_id) : undefined
  const composerRisk = selectedWork?.state === 'awaiting_confirmation' ? '这个工作需要先处理动态中的确认。'
    : selectedWork && ['unknown', 'unverified', 'dispatch_unverified'].includes(selectedWork.state) ? '这个工作结果尚未确认，先查结果再继续。'
    : ''
  const activeWorks = works.filter((work) => taskActive(work.state))

  const changes = data?.changes ?? emptyList
  const placedChanges = useMemo(() => placeChanges(data?.messages ?? [], changes), [data?.messages, changes])

  // Questions sit where they were asked; an answer shows inside its question.
  const questions = data?.questions ?? emptyList
  const folded = useMemo(() => foldAnswers(data?.messages ?? [], questions), [data?.messages, questions])
  const merged = useMemo(() => new Map(jobs.filter((job) => job.state === 'steered').map((job) => [job.message_id, job.outcome])), [jobs])
  const timeline = useMemo(() => chatTimeline(data?.messages ?? [], [
    ...questions.map((question) => ({ key: `question:${question.question_id}`, created_at_ms: question.created_at_ms, waiting: awaitsAnswer(question, workspace.answered, folded.answers), node: <div id={`question-${question.question_id}`} className="scroll-mb-4"><QuestionCard question={question} answer={folded.answers.get(question.question_id)} /></div> })),
  ], merged), [data?.messages, questions, workspace.answered, folded, merged])

  // 重试 sits on the newest reply when it failed or was stopped; it reads the conversation again first and words what it sends by what the reply may have done.
  const newest = data?.messages.at(-1)
  const endedAsk = newest && ['failed', 'stopped', 'interrupted'].includes(newest.status ?? '') ? newest.in_reply_to : undefined
  const retryOf = (messageId: string | undefined) => messageId && !sendDisabled && !jobs.some((job) => job.message_id === messageId && replyUnsettled(job))
    ? async () => { setSendError(null); await workspace.retryReply(messageId) } : undefined
  const retry = retryOf(endedAsk)
  const replied = new Set((data?.messages ?? []).map((message) => message.message_id))
  // A message sent while the bot replies waits behind that reply, so it shows after it until its own turn starts.
  const replying = jobs.some((job) => ['claimed', 'steering', 'running', 'cancelling'].includes(job.state))
  const queued = new Set(replying ? jobs.filter((job) => job.message_id && ['pending_delivery', 'queued', 'steering'].includes(job.state)).map((job) => job.message_id) : [])
  const merging = new Set(jobs.filter((job) => job.state === 'steering').map((job) => job.message_id))
  const entries: ChatEntry[] = []
  const waiting: ChatEntry[] = []
  // The newest reply failed with no 重试, or the newest supplement's outcome is unknown: it can only be read again.
  let requery = false
  for (const message of timeline.messages) {
    const start = entries.length
    for (const aside of timeline.before.get(message.message_id) ?? []) entries.push(aside)
    const schedule = message.role === 'assistant' ? scheduleTrigger(message.in_reply_to) : null
    const detail = jobs.find((job) => job.reply_message_id === message.message_id)?.failure?.detail
    if (schedule) {
      // A schedule run has no message of the person; its reply stands alone and the tasks it started follow it.
      entries.push({ key: message.message_id, message, schedule: scheduleName(schedule, data?.bot_settings, changes), detail })
    } else if (!folded.hidden.has(message.message_id)) entries.push({ ...pendingRow(merged.has(message.message_id) ? { ...message, state: 'merged', merged: merged.get(message.message_id) } : message, message.message_id === newest?.message_id ? retry : undefined), detail })
    const recordedJob = jobs.find((job) => job.reply_message_id === message.message_id || !job.reply_message_id && job.message_id === message.message_id)
    if (message.message_id === newest?.message_id && !retry && (message.status === 'failed' && !recordedJob?.pending_check && (!recordedJob || !canRetryJob(recordedJob)) || merged.has(message.message_id) && (merged.get(message.message_id) ?? 'unknown') === 'unknown')) requery = true
    for (const job of jobs) {
      if (job.message_id !== message.message_id) continue
      if ((job.state !== 'failed' && job.state !== 'rejected') || replied.has(job.reply_message_id ?? '')) continue
      // A reply that ended with no message of its own: 重试 sits on this line while the message is the newest.
      const again = message.role === 'user' && message.message_id === newest?.message_id ? retryOf(message.message_id) : undefined
      entries.push({ key: `job:${job.trigger}`, node: <JobLine job={job} retry={again} refresh={() => workspace.refresh()} /> })
    }
    for (const reply of workspace.interrupted) if (reply.after === message.message_id) entries.push(pendingRow(interruptedRow(reply), message.message_id === newest?.message_id ? retryOf(message.message_id) : undefined))
    if (queued.has(message.message_id)) waiting.push(...entries.splice(start).map((entry) => 'message' in entry && entry.message.message_id === message.message_id ? { ...entry, message: { ...entry.message, state: merging.has(message.message_id) ? 'merging' as const : 'queued' as const } } : entry))
  }
  entries.push(...timeline.rest)
  for (const reply of workspace.interrupted) if (!reply.after || !data?.messages.some((message) => message.message_id === reply.after)) entries.push(pendingRow(interruptedRow(reply)))
  for (const pending of pendingMessages) if (!data?.messages.some((message) => message.message_id === pending.message_id)) entries.push(pendingRow({ message_id: pending.message_id, event_hash: pending.message_id, role: 'user', text: pending.text, created_at_ms: pending.created_at_ms, state: 'sending' }))
  for (const progress of live) entries.push(pendingRow({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0, state: 'streaming' }))

  const owner = data?.conversation?.owner
  const here = workspace.nodes.find((node) => node.device_id === data?.local_node)
  const ownerAway = Boolean(owner && here && (botReadiness(here) === 'ready' || botReadiness(here) === 'unverified') && owner !== here.device_id && !workspace.nodes.some((node) => node.device_id === owner && node.online && !node.revoked))
  const lastUnanswered = data?.messages.filter((message) => unanswered.includes(message.message_id)).at(-1)
  const remote = data?.scheduler?.remote
  // A question waiting on the person is the status already; a spinner under it would say the bot is still thinking.
  const toAnswer = routeRoutine ? [] : questions.filter((question) => awaitsAnswer(question, workspace.answered, folded.answers))
  const asked = toAnswer[0]
  const stopping = activeJobs.some((job) => job.state === 'cancelling')
  if (botBusy && !live.length && !asked) entries.push({ key: 'activity', node: <p role="status" className="flex min-h-7 items-center gap-2 text-sm text-secondary">{bot ? <BotFace bot={bot} mood={workspace.connectionOnline && remote?.reachable !== false ? 'thinking' : 'asleep'} size={24} /> : null}{stopping ? '正在停止' : !workspace.connectionOnline || remote?.reachable === false ? '暂时收不到进展' : '思考中'}</p> })
  entries.push(...waiting)
  // The replies and questions this chat's badge counted when it came on screen start under 新消息.
  const unread = firstUnread(timeline, workspace.fresh, (aside) => 'waiting' in aside && aside.waiting === true && workspace.fresh.has(aside.key))
  const recap = bot ? botRecap({ bot, data, inbox, notifications: workspace.notifications, ignoredNotifications: workspace.ignoredNotifications, workspaceState: workspace.workspaceState, openWorks: workspace.openWorks, fresh: workspace.fresh, connected: workspace.connectionOnline && workspace.dataOrigin === 'network' }) : []
  const recapView = recapPreview(recap)
  const unreadAt = unread ? entries.findIndex((entry) => entry.key === ('key' in unread ? unread.key : unread.message_id)) : -1
  if (unreadAt >= 0) entries.splice(unreadAt, 0, { key: 'unread', node: <p className="flex items-center gap-3 text-sm text-foreground before:h-px before:flex-1 before:bg-foreground/40 after:h-px after:flex-1 after:bg-foreground/40">新消息</p> })

  // Everything waiting on the person lives in the activity drawer; the status line above the composer counts it.
  const reconciling = jobs.filter((job) => job.state === 'reconciling' || job.pending_check)
  const undecided = (pending: PendingMessage) => pending.rejected || pending.delivery === 'unknown' || pending.receipt?.stage === 'unknown'
  const inputStatus = (pending: PendingMessage) => <div key={pending.message_id} className="grid gap-2"><p className="line-clamp-2 text-sm">{pending.text}</p><WorkspaceInputStatus pending={pending} /></div>
  const ownerUnknown = Boolean(remote && !remote.reachable && !ownerAway && newest?.role === 'user')
  const cards = [
    ownerAway && owner && here ? <ActionCard key="owner" icon={WifiOff} title={`${nodeName(owner, lookup)} 离线，对话在等它`} actions={
      <ActionButton variant="primary" disabled={!workspace.writable} action={() => workspace.takeOver()}><ArrowRightLeft aria-hidden="true" />换到 {nodeName(here.device_id, lookup)}</ActionButton>
    } /> : null,
    unanswered.length ? <ActionCard key="unanswered" icon={MessageSquareReply} title={unanswered.length > 1 ? `${unanswered.length} 条消息没等到回复` : '这条消息没等到回复'} actions={<>
      <ActionButton variant="ghost" disabled={!workspace.writable} action={() => workspace.settleUnanswered(unanswered, 'dismiss')}>不回复</ActionButton>
      <ActionButton variant="primary" disabled={!workspace.writable || !agentReady} action={() => workspace.settleUnanswered(unanswered, 'reply')}>{here ? <><Laptop aria-hidden="true" />由 {nodeName(here.device_id, lookup)} 回复</> : '现在回复'}</ActionButton>
    </>}>{lastUnanswered ? <p className="line-clamp-2 text-secondary">{lastUnanswered.text}</p> : null}</ActionCard> : null,
    ...checks.map((check) => <ConfirmCard key={check.check_id} check={check} />),
    remote && ownerUnknown ? <ActionCard key="owner-unknown" icon={Unplug} title={`${nodeName(remote.owner, lookup)} 连不上，回复情况未知`} /> : null,
    ...reconciling.map((job) => <ActionCard key={`reconcile:${job.trigger}`} icon={CircleQuestionMark} title="这次回复的结果待确认" actions={<ActionButton action={() => workspace.refresh()}>再查一次</ActionButton>}><p className="text-secondary">可能已经执行过，先查看结果，再决定是否重试。</p></ActionCard>),
    ...pendingMessages.filter(undecided).map(inputStatus),
  ].filter(Boolean)
  // Waiting tasks surface as question cards and confirming ones as ConfirmCards; the rest are counted here. The owner-unknown card has no action.
  const attention = cards.length - (ownerUnknown ? 1 : 0) + (requery && !reconciling.length ? 1 : 0) + tasks.filter((task) => task.attention && task.state !== 'waiting' && !confirming.has(task.id)).length
  const sending = pendingMessages.filter((pending) => !undecided(pending))
  const changeGroups = [...placedChanges.first, ...[...placedChanges.after.values()].flat()]
  const runtime = jobs.filter((job) => job.runtime).at(-1)?.runtime
  const statusLine = routeRoutine ? '' : chatStatusLine({ attention, tasks: runningTasks, hold: jobs.find((job) => job.state === 'queued')?.hold_reason })

  const notice = !workspace.connectionOnline ? ''
    : workspace.readOnlyNotice ? workspace.readOnlyNotice
    : workspace.writeNotice ? '同步中，稍后可发送' : ''
  // Questions sit where they were asked, so an older one scrolls away; the pill above the composer brings back the first still open.
  const waitingTarget = asked ? `question-${asked.question_id}` : null
  const waitingCount = toAnswer.length
  const targetHidden = useOffscreen(waitingTarget, entries.length)
  const showWaiting = Boolean(waitingTarget && (targetHidden || !workspace.connectionOnline) && !recap.some(item => item.focus === `question:${asked?.question_id}`))
  const reason = composerHint({ ...gate, bot: reach })
  const needsSetup = Boolean(bot && !agentReady)
  const setupDevice = reachDevice(workspace.nodes, reach) ?? ready?.entry.device_id
  const away = reach === 'offline'
  const setupTo = setupDevice ? '/devices/$deviceId' : '/devices'
  const canSetUp = ready?.principal.can_manage === true || ready?.principal.source === 'local_session'

  // This screen stays mounted, hidden, on other routes, so leaving this chat or route closes the drawer and drops its focus return.
  useEffect(() => { setActivityOpen(false); activityFrom.current = null }, [conversationId, pathname])
  // Picked files belong to the bot they were picked for.
  useEffect(() => { setSendError(null); setFiles([]); setSelectedIntent(null); setSelectedTarget('main') }, [conversationId])
  useEffect(() => { if (routeRoutine) setSelectedTarget('main'); setSelectedIntent(null) }, [routeRoutine])
  const location = useHistoryLocation({ workspace, active, focus: routeRoutine ? null : routeMessage, found: entries.some((entry) => entry.key === routeMessage || 'message' in entry && entry.message.message_id === routeMessage) })
  // Nothing pushes a device's bot readiness, so while a device that could reply to an open bot is not ready, read it again every 10s, or every 2s while it starts.
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => () => { for (const section of historySections) latest.current.cancelHistory(section) }, [conversationId, workspace.generation, active])
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
  const chooseInput = (target: string, intent: InputIntent | null) => {
    if (!workspace.isCurrent() || latest.current.conversationId !== conversationId || latest.current.generation !== generation) return
    if (target !== selectedTarget && savingDrafts.current.has(draft)) { setSendError(new Error('草稿正在保存，完成后再切换目标。')); return }
    setSelectedTarget(target); setSelectedIntent(intent)
    if (routeRoutine && target !== selectedTarget && bot) void navigate({ to: '/b/$botId', params: { botId: bot.bot_id }, search: {}, replace: true }).catch(setSendError)
  }
  const setText = (value: string) => setDrafts((current) => { const next = new Map(current).set(conversationId, value); draftStorage.writeStorage(draftKey, Object.fromEntries(next)); return next })
  const welcome = useFirstCooperation(webPlatform.storage, { networkId: ready?.principal.network_id ?? '', deviceId: ready?.principal.device_id ?? '', botId: bot?.bot_id ?? '' }, Boolean(active && !routeRoutine && data?.history.messages.complete && !entries.length))
  const chooseFirst = (example: string) => { if (text.trim() || files.length) return; setText(example); welcome.dismiss(); document.getElementById('message-text')?.focus() }
  const openRecap = (item: BotRecapItem) => {
    const target = item.target
    if (target.kind === 'bot_main' && item.focus === routeMessage) setFocusRequest(value => value + 1)
    if (target.kind === 'task' || target.kind === 'channel_task') void navigate({ to: '/tasks/$taskId', params: { taskId: target.task_id }, search: item.focus ? { message: item.focus } : {} })
    else if (target.kind === 'channel_member') void navigate({ to: '/channels/$channelId', params: { channelId: target.channel_id }, search: item.focus ? { message: item.focus } : {} })
    else if (target.kind === 'work' && bot) void navigate({ to: '/b/$botId/work/$workId', params: { botId: bot.bot_id, workId: target.work_id }, search: item.focus ? { message: item.focus } : {} })
    else if (bot) void navigate({ to: '/b/$botId', params: { botId: bot.bot_id }, search: { ...(target.kind === 'routine' ? { routine: target.trigger } : {}), ...(item.focus ? { message: item.focus } : {}), ...(item.notificationId ? { notification: item.notificationId } : {}) } })
  }

  if (missing) return <section className="conversation-screen" aria-label="对话"><Chat key="missing" entries={[]} empty={<NoBot />} /></section>
  const history = active && bot ? <TaskHistory key={`task-history:${conversationId}`} tasks={tasks} botId={bot.bot_id} attention={attention} open={activityOpen} onOpenChange={setActivityOpen} onOpen={openActivity} from={activityFrom}
    before={<>
      {cards.length || requery ? <ActivitySection title="要处理">{cards}{requery && !reconciling.length ? <div className="grid gap-2"><p className="text-sm text-secondary">再查一次只会更新记录，不会重新执行。</p><ActionButton variant="ghost" className="justify-self-start" disabled={!workspace.connectionOnline} action={() => workspace.refresh()}>再查一次</ActionButton></div> : null}</ActivitySection> : null}
      {sending.length ? <ActivitySection title="发送中">{sending.map(inputStatus)}</ActivitySection> : null}
      {inputTarget ? <InputQueue target={inputTarget} /> : null}
    </>}
    after={<>
      {changeGroups.length ? <ActivitySection title="最近改动">{changeGroups.map((records) => <ChangeCard key={records[0].change_id} records={records} schedules={data?.bot_settings?.schedules ?? []} overwritten={data?.overwritten ?? []} />)}</ActivitySection> : null}
      {data?.sent?.length ? <ActivitySection title="发给其他 Bot">{data.sent.map((sent) => <SentToBot key={sent.message_id} sent={sent} to={workspace.bots.find((item) => item.conversation_id === sent.to) ?? { name: '另一个 Bot' }} />)}</ActivitySection> : null}
      {runtime ? <ActivitySection title="运行信息"><p className="text-sm text-secondary">{runtimeLabel(runtime)}</p></ActivitySection> : null}
    </>} /> : null
  if (bot && !data) return <section className="conversation-screen" aria-label="对话">{history}<ChatSkeleton /></section>

  if (data && historySections.some((section) => !data.history[section].complete)) entries.unshift({ key: 'history-controls', node: <HistoryControls key={`${workspace.generation}:${conversationId}`} workspace={workspace} /> })
  return (
    <section className="conversation-screen" aria-label="对话">
      {history}
      {location.kind !== 'found' ? <div role="status" className="mx-auto flex w-full max-w-3xl flex-wrap items-center gap-2 px-4 py-2 text-sm text-secondary"><p>{location.text}</p>{location.kind === 'failed' ? <Button type="button" variant="secondary" size="sm" onClick={location.retry}>重新定位原记录</Button> : null}</div> : null}
      {!routeRoutine && recapView ? <div aria-label="回来接着办" className="mx-auto grid w-full max-w-3xl gap-1 px-4 py-2 text-sm">
        <Button type="button" variant={recapView.primary.group === 'decision' ? 'secondary' : 'ghost'} size="sm" className="h-auto! justify-start py-2! text-left" onClick={() => openRecap(recapView.primary)}><span className="line-clamp-2 whitespace-normal!">{!workspace.connectionOnline || workspace.dataOrigin === 'cache' ? '最近记录 · ' : ''}<span className="font-medium">{recapView.primary.state}</span> · {recapView.primary.text}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0" /></Button>
        {recapView.more.length ? <Details key={conversationId} label={`其余更新与进行中 · ${recapView.more.length}`}><div className="grid max-h-64 gap-3 overflow-y-auto">{recapGroups.map(({ group, label }) => {
          const items = recapView.more.filter(item => item.group === group)
          return items.length ? <section key={group} aria-label={label} className="grid gap-1"><h2 className="font-medium">{label} · {items.length}</h2>{items.map(item => <Button key={item.key} type="button" variant="ghost" size="sm" className="h-auto! justify-start py-2! text-left" onClick={() => openRecap(item)}><span className="line-clamp-2 whitespace-normal!">{item.state} · {item.text}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0" /></Button>)}</section> : null
        })}</div></Details> : null}
      </div> : null}
      {routeRoutine && bot ? <RoutineRecord workspace={workspace} botId={bot.bot_id} trigger={routeRoutine} notificationId={routeNotification} /> : <Chat
        key={conversationId}
        entries={entries}
        remember
        focus={routeMessage ?? undefined}
        focusRequest={focusRequest}
        empty={bot ? <div className="mx-auto my-auto grid w-full min-w-0 max-w-md justify-items-center gap-4 px-4 py-12 text-center">
          <BotFace bot={bot} mood={botMood(bot, workspace)} size={96} />
          <div className="grid min-w-0 max-w-full justify-items-center gap-1">
            <h2 className="max-w-full text-lg font-medium [overflow-wrap:anywhere]">{bot.name}</h2>
            {workspace.primary === bot.conversation_id ? <PrimaryMark labeled /> : null}
          </div>
          {starting ? <p role="status" className="flex items-center gap-1.5 text-secondary"><LoaderCircle aria-hidden="true" className={`size-4 shrink-0 ${workspace.connectionOnline ? 'animate-spin' : ''}`} />Bot 正在启动</p>
            : needsSetup ? <>
            <p className="flex items-center gap-1.5 text-secondary"><MessageSquareOff aria-hidden="true" className="size-4 shrink-0" />Bot 还不能回复</p>
            {away || canSetUp ? <RoutedAction to={setupTo} params={{ deviceId: setupDevice ?? '' }} search={{ bot: bot.bot_id }}><Bot aria-hidden="true" />{away ? '查看设备' : '设置运行 Bot 的设备'}</RoutedAction>
              : <p className="flex items-center gap-1.5 text-sm text-secondary"><Crown aria-hidden="true" className="size-4 shrink-0" />{setUpOnManager((directory?.members ?? []).filter((member) => member.manage).map((member) => member.name))}</p>}
          </>
            : !text.trim() && !welcome.show ? <div className="grid min-w-0 max-w-full gap-2">
              <div className="flex flex-wrap justify-center gap-2">
                {starterExamples({ project: Boolean(bot.project), attachments: files.length > 0, canRun: agentReady }).map((example) => <button key={example} type="button" className={`${buttonStyles.button} ${buttonStyles.secondary} h-auto max-w-full whitespace-normal! py-2! [overflow-wrap:anywhere]`} onMouseDown={(event) => event.preventDefault()} onClick={() => { setText(example); document.getElementById('message-text')?.focus() }}>{example}</button>)}
              </div>
            </div> : null}
          {welcome.show ? <div className="grid w-full gap-3 text-left">
            <p className="text-sm text-secondary">Bot 已创建 · {workspace.connectionOnline && agentReady ? '执行设备已就绪' : '执行设备待就绪'} · 首条回复尚未收到</p>
            <h3 className="font-medium">先帮我做一件事</h3>
            <div className="flex flex-wrap gap-2">{starterExamples({ project: Boolean(bot.project), attachments: files.length > 0, canRun: agentReady }).map(example => <Button key={example} type="button" variant="secondary" className="h-auto! max-w-full whitespace-normal! py-2!" disabled={Boolean(text.trim() || files.length)} onClick={() => chooseFirst(example)}>{example}</Button>)}</div>
            <h3 className="font-medium">合作约定 <span className="text-sm text-secondary">可选</span></h3>
            <div className="flex flex-wrap gap-2">{cooperationExamples.map(example => <Button key={example.label} type="button" variant="secondary" disabled={Boolean(text.trim() || files.length)} onClick={() => chooseFirst(`请把下面内容保存为这个 Bot 的职责与合作约定：\n\n${example.text}`)}>{example.label}</Button>)}</div>
            <p className="text-sm text-secondary">选择只填入可编辑草稿，由你发送；不会启动任务或增加权限。</p>
            <Button type="button" variant="ghost" size="sm" className="justify-self-start" onClick={welcome.dismiss}>跳过，直接聊天</Button>
          </div> : null}
        </div> : workspace.synced ? <NoBot /> : null}
      />}
      {bot && inputTarget ? <PromptInput
        id="message-text"
        above={!workspace.connectionOnline || history && statusLine || showWaiting ? <>
          {!workspace.connectionOnline ? <ConnectionNotice /> : null}
          {history && statusLine ? <Button type="button" variant="ghost" size="sm" className="max-w-full" onClick={(event) => openActivity(event.currentTarget)}>
            <ListChecks aria-hidden="true" className="size-4 shrink-0 text-secondary" /><span className="min-w-0 truncate">{statusLine}</span>
          </Button> : null}
          {showWaiting ? <Button variant="ghost" size="sm" type="button" className="max-w-full max-md:min-h-11!"
            onClick={() => {
              const card = document.getElementById(waitingTarget ?? '')
              card?.scrollIntoView({ block: 'center', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' })
              // Focus lands on its first choice, where a number answers it; with none, on its answer field.
              card?.querySelector<HTMLElement>('[role="group"] button:not(:disabled), textarea:not(:disabled)')?.focus({ preventScroll: true })
            }}>
            <terms.inbox.icon aria-hidden="true" className="size-4 shrink-0 text-warning" /><span className="min-w-0 truncate">{!workspace.connectionOnline ? '最近记录 · ' : ''}{asked ? questionParts(asked.text).ask : waitingCount > 1 ? `${waitingCount} 项${terms.inbox.label}` : terms.inbox.label}</span><ArrowDown aria-hidden="true" className="size-3.5 shrink-0 text-secondary" />
          </Button> : null}
        </> : null}
        value={text}
        hint={notice}
        reason={reason}
        disabled={sendDisabled}
        label={`${targetAction} ${targetLabel}`}
        sendLabel={inputIntentLabel(intent, targetBusy)}
        target={<div className="flex w-full min-w-0 flex-wrap items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-sm text-secondary" title={`${targetAction} ${targetLabel}`}>{targetAction} · {targetLabel}</span>
          <Popover>
            <PopoverTrigger asChild><Button type="button" variant="secondary" size="sm">发送方式与目标<ChevronDown aria-hidden="true" className="size-4" /></Button></PopoverTrigger>
            <PopoverContent align="end" className="grid max-h-(--radix-popover-content-available-height) gap-4 overflow-y-auto">
              <section aria-label="发送方式" className="grid gap-1">
                <h3 className="px-3 text-sm text-secondary">发送方式</h3>
                {inputIntents.map((item) => <PopoverClose key={item.intent} asChild><Button type="button" variant={intent === item.intent ? 'secondary' : 'ghost'} size="sm" className="justify-start" aria-pressed={intent === item.intent} onClick={() => chooseInput(selectedTarget, item.intent)}>{item.label}</Button></PopoverClose>)}
              </section>
              {activeWorks.length ? <section aria-label="进行中的工作" className="grid min-h-0 gap-1">
                <h3 className="px-3 text-sm text-secondary">进行中的工作 · {activeWorks.length}</h3>
                <div className="grid max-h-60 gap-1 overflow-y-auto">
                  {activeWorks.map((work) => <PopoverClose key={work.work_id} asChild><Button type="button" variant={selectedTarget === `work:${work.work_id}` ? 'secondary' : 'ghost'} size="sm" className="h-auto! min-w-0 justify-start py-2! [&_span]:min-w-0 [&_span]:max-w-full" aria-pressed={selectedTarget === `work:${work.work_id}`} title={work.title || '工作'} onClick={() => chooseInput(`work:${work.work_id}`, 'continue')}><span className="line-clamp-2 whitespace-normal! text-start">补充：{work.title || '工作'}</span></Button></PopoverClose>)}
                </div>
              </section> : null}
              {selectedTarget !== 'main' ? <PopoverClose asChild><Button type="button" variant="ghost" size="sm" onClick={() => chooseInput('main', null)}>回到主聊天</Button></PopoverClose> : null}
            </PopoverContent>
          </Popover>
        </div>}
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
          workspace.sendMessage(text, submittedFiles, undefined, { target: inputTarget, intent, conversation_id: conversationId, onSaved: (input) => { if (!workspace.isCurrent() || latest.current.conversationId !== conversationId || latest.current.view !== "chat" || draftRef.current !== submittedDraft) return; if (input.target.kind === "task" || input.target.kind === "channel_task") void navigate({ to: "/tasks/$taskId", params: { taskId: input.target.task_id } }) } }).then((sent) => { if (sent && workspace.isCurrent()) { if (latest.current.conversationId !== conversationId || draftRef.current === submittedDraft) clearSentDraft(conversationId, text); if (draftRef.current === submittedDraft) { setFiles((current) => current === submittedFiles ? [] : current); setSelectedIntent(null) } } }).catch(setSendError).finally(() => { savingDrafts.current.delete(submittedDraft) })
        }}
      >
        <InlineError error={sendError} className="basis-full" />
        {composerRisk ? <p className="basis-full text-warning">{composerRisk}</p> : null}
        {inputTarget.kind === 'routine' ? <Button type="button" variant="ghost" size="sm" onClick={() => chooseInput('main', null)}>改回主聊天</Button> : null}
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

type TaskRow = { id: string; state: string; attention: boolean; device: string; work: WorkRecord }

function TaskIcon({ state, stale, away = false }: { state: string; stale: boolean; away?: boolean }) {
  const failed = ['failed', 'rejected', 'dispatch_rejected'].includes(state)
  const Icon = stale ? CircleQuestionMark : away ? WifiOff : state === 'completed' ? CircleCheck
    : failed ? CircleX : state === 'interrupted' ? CircleStop
    : state === 'waiting' ? terms.approval.icon : state === 'awaiting_children' ? Hourglass
    : taskActive(state) ? LoaderCircle : CircleQuestionMark
  return <Icon aria-hidden="true" className={`size-4 shrink-0 ${state === 'completed' ? 'text-success' : failed ? 'text-danger' : state === 'waiting' ? 'text-warning' : !stale && !away && taskActive(state) ? 'animate-spin text-info motion-reduce:animate-none' : 'text-secondary'}`} />
}

function ActivitySection({ title, children }: { title: string; children: ReactNode }) {
  return <section className="grid gap-3"><h3 className="text-sm font-medium text-secondary">{title}</h3>{children}</section>
}

/** The header's 动态 drawer: what waits on the person and what is in flight (`before`), this chat's tasks, then records (`after`). */
function TaskHistory({ tasks, botId, attention, open, onOpenChange, onOpen, from, before, after }: { tasks: TaskRow[]; botId: string; attention: number; open: boolean; onOpenChange: (open: boolean) => void; onOpen: (from: HTMLElement | null) => void; from: RefObject<HTMLElement | null>; before: ReactNode; after: ReactNode }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const trigger = useRef<HTMLButtonElement>(null)
  useEffect(() => () => { from.current = null }, [])
  const leave = () => { from.current = null; onOpenChange(false) }
  return <>
    <PageActions><Button ref={trigger} type="button" variant="ghost" size="sm" aria-label={attention ? `动态，${attention} 项要处理` : '动态'} title={attention ? `动态，${attention} 项要处理` : '动态'} onClick={() => onOpen(trigger.current)}><span className="relative inline-flex"><ListChecks aria-hidden="true" />{attention ? <Dot tone="warning" className="absolute -end-1 -top-1 ring-2 ring-surface" /> : null}</span></Button></PageActions>
    <Drawer open={open} onOpenChange={onOpenChange}>
      <DrawerContent title="动态" className="!w-full min-w-0 gap-0 sm:!max-w-md" onCloseAutoFocus={(event) => { event.preventDefault(); if (from.current) (from.current.isConnected ? from.current : trigger.current)?.focus({ preventScroll: true }) }}>
        <div className="grid content-start gap-6">
          {workspace.connectionOnline && workspace.incomplete ? <p role="status" className="text-sm text-secondary">同步中</p> : null}
          {before}
          {!workspace.data ? <p role="status" className="text-sm text-secondary">正在读取</p> : tasks.length ? <ActivitySection title="任务"><ul className="divide-y">{tasks.map((task) => {
            const stale = taskStale(task.state, workspace.connectionOnline) || !workspace.connectionOnline && task.state === 'awaiting_children'
            const away = task.state === 'dispatch_waiting' && workspace.nodes.some((node) => node.device_id === task.device && !node.online)
            return <li key={task.id}><Link to="/b/$botId/work/$workId" params={{ botId, workId: task.id }} onClick={leave} className="-mx-2 flex min-h-11 min-w-0 items-start gap-2 rounded-control px-2 py-3 hover:bg-surface-muted/60 active:bg-surface-muted">
              <TaskIcon state={task.state} stale={stale} away={away} />
              <span className="grid min-w-0 flex-1 gap-1"><span className="break-words font-medium">{task.work.title || '任务'}</span><span className="flex min-w-0 flex-wrap gap-x-1 text-sm text-secondary"><span>{stale ? staleTaskLabel : away ? taskCopy.away : taskLabel(task.state)}</span><span aria-hidden="true">·</span><span className="min-w-0 break-words">{nodeName(task.device, { nodes: workspace.nodes, directory })}</span><Elapsed dot start={task.work.dispatched_at_ms} end={task.work.finished_at_ms} active={(taskActive(task.state) || task.state === 'waiting') && !stale} /></span></span>
              <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-secondary" />
            </Link></li>
          })}</ul></ActivitySection> : null}
          {after}
          <RoutedAction variant="secondary" className="justify-self-start" to="/tasks" onClick={leave}>查看任务对话</RoutedAction>
        </div>
      </DrawerContent>
    </Drawer>
  </>
}

/** Shaped like a message from another bot, its name above the text, so both directions read as one kind of thing. */
function SentToBot({ sent, to }: { sent: SentBotMessage; to: { name: string; bot_id?: string } }) {
  return <div className="flex min-w-0 flex-col items-start gap-1">
    <p className="flex min-w-0 max-w-full items-center gap-1.5 text-sm text-secondary">
      <Send aria-hidden="true" className="size-4 shrink-0" />
      <span className="shrink-0">发给</span>
      {to.bot_id ? <Link to="/b/$botId" params={{ botId: to.bot_id }} className="hit-area relative flex min-w-0 font-medium text-foreground underline-offset-4 hover:underline"><span className="truncate">{to.name}</span></Link>
        : <span className="min-w-0 truncate font-medium text-foreground">{to.name}</span>}
    </p>
    <p className="line-clamp-3 min-w-0 max-w-[85%] whitespace-pre-wrap rounded-panel border px-4 py-2 [overflow-wrap:anywhere] sm:max-w-[75%]" title={sent.text}>{sent.text}</p>
  </div>
}

/** `retry`: 重试, only offered while its message is the newest. */
function JobLine({ job, retry, refresh }: { job: ManagerJob; retry?: () => Promise<void>; refresh: () => Promise<void> }) {
  return <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm">
    <span className="flex shrink-0 items-center gap-1 text-danger"><CircleX aria-hidden="true" className="size-4 shrink-0" />没能回复</span>
    {job.failure ? <span className="min-w-0 truncate text-secondary" title={job.failure.detail}>{failureText(job.failure.kind) || failureReason(job.failure.detail)}</span> : null}
    {retry ? <ActionButton variant="ghost" className="-ms-1" action={retry}><RotateCcw aria-hidden="true" />重试</ActionButton> : <ActionButton variant="ghost" action={refresh}>再查一次</ActionButton>}
  </div>
}


function RoutineRecord({ workspace, botId, trigger, notificationId }: { workspace: ReturnType<typeof useWorkspace>; botId: string; trigger: string; notificationId: string | null }) {
  const record = useNotificationRecord({ workspace, conversationId: workspace.conversationId, target: { kind: 'routine', bot_id: botId, trigger }, notificationId })
  const state = routineState(workspace.data, trigger, workspace.connectionOnline && workspace.dataOrigin === 'network')
  return <div className="mx-auto grid w-full max-w-3xl gap-3 overflow-y-auto px-4 py-6" aria-label="原例行记录">
    <h2 className="text-sm font-medium">{scheduleName(trigger, workspace.data?.bot_settings, workspace.data?.changes)}</h2>
    {state ? <p role="status" className="text-sm text-secondary">{state} · 原例行执行记录</p> : null}
    {record.kind === 'record' ? record.content.kind === 'question' ? <QuestionCard question={record.content.question} /> : <Chat entries={[{ key: record.content.message.message_id, message: record.content.message }]} empty={null} /> : <p role="status" className="text-sm text-secondary">{!notificationId && state ? '尚未读取到本轮的结果正文或问题；执行记录不代表结果已验收。' : record.text}</p>}
    {record.kind === 'failed' ? <Button type="button" variant="secondary" onClick={record.retry}>重新读取原记录</Button> : null}
  </div>
}
