import { historySections } from '@vibeface/client/workspace/model.ts'
import { HistoryControls } from '@/features/chat/history-controls'
import { Link, Stack, useFocusEffect, useLocalSearchParams, useRouter } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { ArrowDown, Bot, ChevronRight, Crown, ListChecks, LoaderCircle, MessageSquareOff, Search, Settings, Star } from 'lucide-react-native'
import { useCallback, useEffect, useMemo, useRef, useState, type ComponentProps } from 'react'
import { FlatList, Modal, Platform, ScrollView, Pressable, StyleSheet, View, type NativeScrollEvent } from 'react-native'
import { KeyboardChatScrollView, KeyboardStickyView, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Animated, { useAnimatedStyle, useSharedValue } from 'react-native-reanimated'
import { useSafeAreaFrame, useSafeAreaInsets } from 'react-native-safe-area-context'
import { useCSSVariable, withUniwind } from 'uniwind'
import { awaitsDevice, botReach, botCanSend, composerHint, inputTargetKey, inputIntents, inputIntentLabel, promptIntent, replyUnsettled, starterExamples, reachDevice } from '@vibeface/client/chat/protocol.ts'
import { useNotificationRecord } from '@vibeface/client/chat/use-notification-record.ts'
import { useHistoryLocation } from '@vibeface/client/chat/use-history-location.ts'
import { chatUpdates, messageTimes } from '@vibeface/client/chat/timeline.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { setUpOnManager } from '@vibeface/client/identity/copy.ts'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { ErrorText } from '@/components/action'
import { botCopy, scheduleName } from '@vibeface/client/workspace/bot-settings.ts'
import { unreachable } from '@vibeface/client/workspace/home.ts'
import { questionOpen, questionParts } from '@vibeface/client/workspace/questions.ts'
import { chatStatusLine } from '@vibeface/client/workspace/labels.ts'
import { botMood, BotFace, ScreenFaces, type FaceBot } from '@/components/bot-face'
import { OfflineBar } from '@/components/connection-pill'
import { EmptyState } from '@/components/kit'
import { PopoverPanel } from '@/components/arc/popover'
import { privateFiles } from '@/platform/files'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import type { Attachment, ChatMessage, Conversation } from '@/data/types'
import type { InputIntent, InputTarget, Question } from '@vibeface/client/workspace/types.ts'
import type { ReadAttachment } from '@/features/chat/attachments'
import { Composer } from '@/features/chat/composer'
import { Message } from '@/features/chat/message'
import { useChatDraftOwner, useDraftView, type ChatDraftOwner } from '@/features/chat/use-chat-draft'
import { chatDrafts } from '@/platform/native-chat-drafts'
import { readingPositions } from '@/platform/reading-position'
import { chatMessageLocation, visibleChatMessageIds, type VisibleChatMessage } from '@/platform/chat-location'
import { QuestionCard } from '@/features/chat/question-card'
import { useChatSummary, type Row } from '@/features/chat/work'
import { RecoveryAlert } from '@/features/recovery/recovery'
import { touchRow, useWide } from '@/lib/layout'
import { fadeIn } from '@/lib/motion'
import { mobilePlatform } from '@/platform'

/** One row of the chat: a message, or a question or status line between them. */
type Entry = { key: string; message: ChatMessage; time?: string; grouped: boolean } | (Row & { attached?: boolean })

const Fade = withUniwind(Animated.View)

function awayFromLatest(event: NativeScrollEvent, inset: Pick<NativeScrollEvent['contentInset'], 'top' | 'bottom'>, ios: boolean) {
  const { contentOffset, contentInset, contentSize, layoutMeasurement } = event
  const padding = ios ? contentInset : inset
  const range = Math.max(0, contentSize.height + padding.top + padding.bottom - layoutMeasurement.height)
  const distance = contentOffset.y + (ios ? contentInset.top : 0)
  return contentSize.height > 0 && layoutMeasurement.height > 0 && Math.min(Math.max(0, distance), range) > 64
}

/**
 * Only messages and questions: questions sit right before the first message written after them, 新消息 above what the badge counted and a
 * reply that never came as one line after its message; the newest entry is first for the inverted list. The rest is on the activity page.
 */
function entries(chat: Conversation, failures: ReadonlyMap<string, Row[]>, bot: FaceBot, offline: boolean): Entry[] {
  const rows: Entry[] = []
  const question = (item: Question): Entry => ({ key: `q:${item.question_id}`, node: <QuestionCard question={item} answer={chat.answers.get(item.question_id)} /> })
  // The streamed reply and the final one share a row key, so the row updates in place instead of remounting and fading in again.
  const turns = new Set<string>()
  const keyOf = (message: ChatMessage) => {
    if (message.role !== 'assistant' || !message.in_reply_to || turns.has(message.in_reply_to)) return message.message_id
    turns.add(message.in_reply_to)
    return `turn:${message.in_reply_to}`
  }
  // A streamed reply whose final text is already in the history is the same reply: draw it once, whichever side is behind.
  const same = (a: string, b: string) => a.trim() !== '' && b.trim() !== '' && (a.trim().startsWith(b.trim()) || b.trim().startsWith(a.trim()))
  const messages = chat.messages.filter((message) => message.state !== 'streaming' || !message.in_reply_to || !chat.messages.some((other) => other.role === 'assistant'
    && other.state !== 'streaming' && other.in_reply_to === message.in_reply_to && same(other.text, message.text)))
  const times = messageTimes(messages.filter((message) => !message.state && !chat.hidden.has(message.message_id)), Date.now())
  for (const message of messages) {
    rows.push(...(chat.before.get(message.message_id) ?? []).map(question))
    const previous = rows.at(-1)
    const grouped = previous !== undefined && 'message' in previous && !previous.time && previous.message.role === message.role
    if (!chat.hidden.has(message.message_id)) rows.push({ key: keyOf(message), message, time: times.get(message.message_id), grouped })
    rows.push(...(failures.get(message.message_id) ?? []).map((row) => ({ ...row, attached: true })))
  }
  rows.push(...chat.rest.map(question))
  const unreadAt = rows.findIndex((row) => ('message' in row ? row.message.message_id : row.key) === chat.unread)
  if (unreadAt >= 0) rows.splice(unreadAt, 0, { key: 'unread', node: <View className="flex-row items-center gap-3"><View className="h-px flex-1 bg-foreground/40" /><Text className="text-sm text-foreground">新消息</Text><View className="h-px flex-1 bg-foreground/40" /></View> })
  if (chat.replying && !chat.messages.some((message) => message.state === 'streaming')) rows.push({
    key: 'activity',
    node: <Fade entering={fadeIn} accessibilityRole="progressbar" className="min-h-7 flex-row items-center gap-2"><BotFace bot={bot} mood={offline ? 'asleep' : 'thinking'} size={24} /><Text className="text-sm text-muted">{chat.status ?? '思考中'}</Text></Fade>,
  })
  return rows.reverse()
}

/** Inverted, so the leading item is the lower one: a card caused by the message above it stays close to it. */
function Separator({ leadingItem }: { leadingItem: Entry }) {
  return <View className={'grouped' in leadingItem && leadingItem.grouped ? 'h-2' : 'attached' in leadingItem && leadingItem.attached ? 'h-3' : 'h-6'} />
}

/** The composer reads the draft itself, so typing re-renders it and not the chat above it. */
function DraftComposer({ draft, selectedIntent, targetBusy, onChooseIntent, children, ...props }: { draft: ChatDraftOwner; selectedIntent: InputIntent | null; targetBusy: boolean; onChooseIntent: () => void } & Omit<ComponentProps<typeof Composer<Attachment>>, 'value' | 'onChange' | 'files' | 'onFiles' | 'onPickFiles' | 'sendLabel' | 'target'>) {
  const { text, files, error, snapshot } = useDraftView(draft)
  const label = inputIntentLabel(selectedIntent ?? promptIntent(text), targetBusy)
  return <Composer {...props} sendLabel={label} target={<Button variant="ghost" size="sm" accessibilityLabel={`输入方式：${label}`} onPress={onChooseIntent}><Text className="text-sm">{label}</Text></Button>} value={text} onChange={draft.changeText} files={files} onFiles={draft.changeFiles} onPickFiles={draft.addFiles}>
    {error || snapshot.missingFiles.length ? <Text accessibilityRole="alert" className="text-sm text-danger">{error || `附件 ${snapshot.missingFiles.join('、')} 已不可用，请重新选择。`}</Text> : null}
    {children}
  </Composer>
}

function Starters({ draft, project, canRun }: { draft: ChatDraftOwner; project: boolean; canRun: boolean }) {
  const { text, files } = useDraftView(draft)
  return text.trim() ? null : <View pointerEvents="box-none" className="flex-row flex-wrap justify-center gap-2">
    {starterExamples({ project, attachments: files.length > 0, canRun }).map((example) => <Button key={example} variant="secondary" className="rounded-pill" onPress={() => draft.changeText(example)}><Text className="text-muted">{example}</Text></Button>)}
  </View>
}

export default function ChatScreen() {
  const router = useRouter()
  const { botId, routine, message: targetMessage, notification } = useLocalSearchParams<{ botId: string; routine?: string; message?: string; notification?: string }>()
  const workspace = useWorkspace()
  const device = useDevice()
  const insets = useSafeAreaInsets()
  // Only iOS draws the bar over the list; Android's stays opaque above it.
  const ios = Platform.OS === 'ios'
  const barHeight = useHeaderHeight()
  const header = ios ? barHeight : 0
  // iOS sizes a custom title against the whole bar, so a long name or large text ran under the three header buttons; the
  // screen's width less the sidebar, the back button and that button group keeps it truncated before them. Android's
  // title slot already ends where the bar's buttons begin.
  const frame = useSafeAreaFrame().width
  const wide = useWide()
  // Compact iPad windows also need leading room for the system window controls.
  const titleWidth = Platform.OS === 'ios' ? (wide ? frame - 320 : frame) - (Platform.isPad && !wide ? 328 : 248) : undefined
  const background = useCSSVariable('--background')
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const id = bot?.conversation_id
  const chat = id ? workspace.conversations[id] : undefined
  const live = useLive()
  const liveGeneration = live?.generation
  const latestWorkspace = useRef(workspace)
  latestWorkspace.current = workspace
  const [intentMenu, setIntentMenu] = useState<{ key: string; version: number } | null>(null)
  const intentMenuVersion = useRef(0)
  const intentMenuOwner = useRef(intentMenu)
  intentMenuOwner.current = intentMenu
  const [selectedIntent, setSelectedIntent] = useState<InputIntent | null>(null)
  const [selectedTarget, setSelectedTarget] = useState(routine ? `routine:${routine}` : 'main')
  const draftScope = { networkId: workspace.networkId ?? '', deviceId: workspace.deviceId ?? '', conversationId: id ?? '', inputTarget: selectedTarget }
  const draftScopeKey = JSON.stringify(draftScope)
  const draft = useChatDraftOwner(draftScope)
  const [sendError, setSendError] = useState('')
  const [sendingScope, setSendingScope] = useState<string | null>(null)
  const savingDrafts = useRef(new Set<string>())
  const currentScope = useRef(draftScopeKey)
  currentScope.current = draftScopeKey
  useEffect(() => { setSelectedTarget(routine ? `routine:${routine}` : 'main'); setSelectedIntent(null); setIntentMenu(null) }, [id, routine])
  const [routeFocused, setRouteFocused] = useState(false)
  const routeActive = useRef(false)
  useFocusEffect(useCallback(() => { routeActive.current = true; setRouteFocused(true); const close = id ? latestWorkspace.current.actions.open(id) : undefined; return () => { routeActive.current = false; setRouteFocused(false); intentMenuOwner.current = null; setIntentMenu(null); close?.() } }, [id, live?.generation]))
  const extras = useChatSummary(id ?? '')
  // As on the web: until a device can reply, the empty chat says so and points a manager at the AI settings instead of offering starters.
  const reach = live ? botReach(live.nodes) : 'ready'
  const canSetUp = device.ready?.principal.can_manage === true
  // As on the web: the device to set up, or the one that is away; this phone sets up only the computer it connects through, any other opens its page.
  const target = live ? reachDevice(live.nodes, reach) : undefined
  const entry = device.ready?.entry.device_id
  const setupHref = reach === 'offline' ? (target ? `/device/${target}` as const : '/settings') : target && target !== entry ? `/device/${target}` as const : '/ai'
  // Nothing pushes a device's bot readiness, so while a device that could reply is not ready and this chat is on screen, read it again every 10s, or every 2s while it starts.
  const latestLive = useRef(live)
  latestLive.current = live
  useFocusEffect(useCallback(() => () => { for (const section of historySections) latestLive.current?.cancelHistory(section) }, [id, live?.generation]))
  const awaiting = live ? awaitsDevice(reach) : false, starting = reach === 'starting'
  useFocusEffect(useCallback(() => {
    if (!awaiting) return
    const read = () => { if (latestLive.current && !latestLive.current.refreshing) void latestLive.current.refresh().catch(() => {}) }
    read()
    const timer = setInterval(read, starting ? 2000 : 10000)
    return () => clearInterval(timer)
  }, [awaiting, starting]))
  const keyboard = useReanimatedKeyboardAnimation()
  const dockHeight = useSharedValue(0)
  const list = useRef<FlatList<ReturnType<typeof entries>[number]>>(null)
  const scrollInset = useRef({ id, top: 0, bottom: 0 })
  if (scrollInset.current.id !== id) scrollInset.current = { id, top: 0, bottom: 0 }
  const [scrollButton, setScrollButton] = useState({ id, visible: false })
  const away = useRef(false)
  const positions = useMemo(() => readingPositions(mobilePlatform.storage, { networkId: workspace.networkId ?? '', deviceId: workspace.deviceId ?? '' }), [workspace.networkId, workspace.deviceId])
  const positionKey = JSON.stringify([workspace.networkId, workspace.deviceId, id, targetMessage])
  const restoration = useRef<{ key: string; messageId: string | null; done: boolean }>({ key: '', messageId: null, done: false })
  if (restoration.current.key !== positionKey) {
    const saved = id ? positions.get(id) : null
    restoration.current = { key: positionKey, messageId: targetMessage ?? (saved?.kind === 'message' ? saved.messageId : null), done: false }
  }
  const offline = unreachable(device.link)
  // Times say the date once it is no longer today, so a new day builds the rows again too.
  const day = new Date().toDateString()
  const listed = useMemo(() => chat && bot ? entries(chat, extras.failures, bot, offline) : [], [chat, extras.failures, bot, offline, day])
  const history = live && live.conversationId === id ? live.data?.history : undefined
  const rows: Entry[] = history && live && (!history.messages.complete || !history.questions.complete) ? [...listed, { key: 'history-controls', node: <HistoryControls key={`${live.generation}:${id}`} workspace={live} sections={['messages', 'questions']} /> }] : listed
  const [visible, setVisible] = useState<{ id: string; messages: VisibleChatMessage[] }>({ id: '', messages: [] })
  const knownMessages = useMemo(() => new Map(chat?.messages.map((message) => [message.message_id, message.event_hash]) ?? []), [chat?.messages])
  const reading = useRef({ id, positions, live, knownMessages })
  reading.current = { id, positions, live, knownMessages }
  const viewability = useRef((event: Parameters<NonNullable<ComponentProps<typeof FlatList<Entry>>['onViewableItemsChanged']>>[0]) => {
    const current = reading.current
    if (!current.id || !routeActive.current) return
    const messages = event.viewableItems.filter((token) => token.isViewable && 'message' in token.item).map((token) => token.item).filter((item): item is Extract<Entry, { message: ChatMessage }> => 'message' in item && current.knownMessages.get(item.message.message_id) === item.message.event_hash)
    const observed = messages.filter((item) => !item.message.state).map((item) => ({ message_id: item.message.message_id, event_hash: item.message.event_hash }))
    setVisible((previous) => previous.id === current.id && JSON.stringify(previous.messages) === JSON.stringify(observed) ? previous : { id: current.id ?? '', messages: observed })
    if (!restoration.current.done) return
    const anchor = messages.at(-1)?.message.message_id
    current.positions.save(current.id, away.current && anchor ? { kind: 'message', messageId: anchor } : { kind: 'latest' })
  })
  useEffect(() => {
    if (routeActive.current && visible.id === id && live?.view === 'chat' && live.conversationId === id && live.dataOrigin === 'network') void live.markMessagesRead(visibleChatMessageIds(visible.messages, live.data?.messages ?? [])).catch((cause) => { if (routeActive.current && reading.current.id === id) setSendError(messageOf(cause)) })
  }, [id, visible, live?.view, live?.conversationId, live?.data, live?.notifications, live?.generation])
  const loadOlder = () => {
    const current = reading.current
    if (!routeActive.current || !current.id || current.live?.conversationId !== current.id || current.live.dataOrigin !== 'network' || !current.live.connectionOnline || current.live.historyLoading.has('messages') || current.live.data?.history.messages.complete) return
    void current.live.loadHistory('messages').catch((cause) => { if (reading.current.id === current.id) setSendError(messageOf(cause)) })
  }
  const focused = restoration.current.messageId
  const questionIds = [...chat?.before.values() ?? [], chat?.rest ?? []].flat().map((question) => question.question_id)
  const recordLocation = focused ? chatMessageLocation({ messageId: focused, messages: chat?.messages ?? [], questionIds }) : null
  const locatedIndex = rows.findIndex((row) => recordLocation?.kind === 'question' ? row.key === `q:${recordLocation.questionId}` : recordLocation?.kind === 'message' && 'message' in row && row.message.message_id === recordLocation.messageId)
  const location = useHistoryLocation({ workspace: live, active: Boolean(routeFocused && id && !routine && focused), focus: recordLocation?.kind === 'question' ? `question:${recordLocation.questionId}` : focused, found: locatedIndex >= 0 })
  useEffect(() => {
    if (!id || restoration.current.done) return
    if (!focused) restoration.current.done = true
    else if (locatedIndex >= 0) {
      restoration.current.done = true
      away.current = locatedIndex > 1
      requestAnimationFrame(() => list.current?.scrollToIndex({ index: locatedIndex, viewPosition: 1, animated: false }))
    }
  }, [positionKey, focused, locatedIndex])
  const toLatest = () => {
    if (scrollInset.current.id !== id) return
    restoration.current.done = true
    away.current = false
    if (id) positions.save(id, { kind: 'latest' })
    setScrollButton({ id, visible: false })
    list.current?.scrollToOffset({ offset: ios ? -scrollInset.current.top : 0, animated: true })
  }
  // Opening a chat with 新消息 brings it to the top of the screen, the unread under it.
  const unreadIndex = useRef(-1)
  const toUnread = (index: number) => { if (scrollInset.current.id === id) list.current?.scrollToIndex({ index, viewPosition: 1, animated: false }) }
  useEffect(() => {
    const index = unreadIndex.current
    if (index >= 0 && !restoration.current.messageId) requestAnimationFrame(() => toUnread(index))
  }, [id, Boolean(chat?.unread)])
  // The empty state stays centred in the space above the composer instead of riding up with the list.
  const above = useAnimatedStyle(() => ({ paddingBottom: ios
    ? dockHeight.value + Math.max(0, Math.abs(keyboard.height.value) - insets.bottom * keyboard.progress.value)
    : Math.max(0, Math.abs(keyboard.height.value) - insets.bottom) }))
  // Rows get callbacks that keep their identity and read the newest draft and workspace, so history rows skip re-rendering.
  const latestDraft = useRef(draft)
  latestDraft.current = draft
  const quote = useCallback((message: ChatMessage) => {
    const text = latestDraft.current.view().text
    latestDraft.current.changeText(`${text ? `${text}\n\n` : ''}> ${message.text.replace(/\n/g, '\n> ')}\n\n`)
  }, [])
  const readAttachment = useCallback<ReadAttachment>((attachment, offset) => latestLive.current
    ? latestLive.current.readArtifactBytes(attachment.hash, offset, id) : Promise.reject(new Error('连上后可读取')), [id])
  const attachmentScope = useMemo(() => JSON.stringify([workspace.networkId, workspace.deviceId, id]), [workspace.networkId, workspace.deviceId, id])
  const blocked = chat?.blocked ?? (chat ? null : '尚未完成首读')
  // 重试 on the newest reply once it failed or was stopped, as on the web.
  const newest = chat?.messages.at(-1)
  const asked = newest && ['failed', 'stopped'].includes(newest.status ?? '') ? newest.in_reply_to : undefined
  const unknown = live?.data?.scheduler?.jobs.some((job) => job.message_id === asked && replyUnsettled(job))
  const retryAsked = useCallback(() => latestLive.current && asked ? latestLive.current.retryReply(asked) : Promise.resolve(), [asked])
  const retry = asked && live && !blocked && !unknown ? retryAsked : undefined
  const read = live ? readAttachment : undefined
  const renderItem = useCallback(({ item }: { item: Entry }) => !('message' in item) ? <>{item.node}</> : bot
    ? <Message message={item.message} bot={bot} time={item.time} offline={offline} retry={item.message === newest ? retry : undefined} attachmentScope={attachmentScope} readAttachment={read} onQuote={quote} />
    : null, [bot, offline, newest, retry, attachmentScope, read, quote])

  if (!bot || !id) return <EmptyState icon={Bot} title={botCopy.notFound} className="flex-1 justify-center" />
  const routineTrigger = selectedTarget.startsWith('routine:') ? selectedTarget.slice(8) : null
  const selectedWork = selectedTarget.startsWith('work:') ? live?.data?.works.find((work) => work.work_id === selectedTarget.slice(5)) : undefined
  const currentJob = live?.data?.scheduler?.jobs.find((job) => ['running', 'claimed', 'steering'].includes(job.state))
  const currentInput = chat?.messages.find((message) => message.message_id === currentJob?.message_id)
  const targetOption: { label: string; target: InputTarget } = selectedTarget.startsWith('work:') ? { label: selectedWork?.title || '原工作', target: { kind: 'work', work_id: selectedTarget.slice(5) } } : routineTrigger ? { label: `例行任务 · ${scheduleName(routineTrigger, live?.data?.bot_settings, live?.data?.changes)}`, target: { kind: 'routine' as const, bot_id: bot.bot_id, trigger: routineTrigger } } : { label: currentInput?.text.slice(0, 48) || `${bot.name} · 主聊天`, target: { kind: 'bot_main' as const, bot_id: bot.bot_id } }
  const inputTarget = targetOption.target
  const targetBusy = inputTarget.kind === 'routine' ? Boolean(live?.data?.scopeJobs.some((job) => job.trigger === inputTarget.trigger && ['queued', 'steering', 'claimed', 'running'].includes(job.state))) : inputTarget.kind === 'work' ? Boolean(selectedWork && ['accepted', 'starting', 'running'].includes(selectedWork.state)) : Boolean(chat?.replying)
  const intentMenuKey = JSON.stringify([draftScopeKey, liveGeneration])
  const changeTarget = async (inputTarget: string) => {
    const scope = draftScopeKey
    const current = () => routeActive.current && currentScope.current === scope && latestLive.current?.generation === liveGeneration
    const snapshot = await draft.flush()
    if (!current()) return
    if (snapshot.text || snapshot.files.length) {
      const next = { ...draftScope, inputTarget }
      const existing = chatDrafts.read(next)
      if (existing.text || existing.files.length) throw new Error('目标已有草稿，请先处理它，当前草稿已保留。')
      const lease = chatDrafts.lease(snapshot)
      try {
        await privateFiles.run(async (valid) => {
          if (!valid() || !current()) return
          const saved = await chatDrafts.write(next, { text: snapshot.text, files: snapshot.files })
          if (valid() && current() && !await draft.clear(snapshot)) {
            await chatDrafts.clear(next, saved)
            throw new Error('草稿刚有补充，请重新选择目标；新内容已保留。')
          }
        })
      } finally { lease.release() }
    }
    if (current()) { setSelectedTarget(inputTarget); setSelectedIntent(null) }
  }
  const intentOptions = [
    ...inputIntents.map((item) => ({ text: item.label, action: () => setSelectedIntent(item.intent) })),
    ...(live?.data?.works.filter((work) => ['accepted', 'starting', 'running'].includes(work.state)) ?? []).map((work) => ({ text: `补充：${work.title || '工作'}`, action: () => { void changeTarget(`work:${work.work_id}`).catch((cause) => { if (currentScope.current === draftScopeKey) setSendError(messageOf(cause)) }) } })),
    ...(selectedTarget !== 'main' ? [{ text: '回到主聊天', action: () => { void changeTarget('main').catch((cause) => { if (currentScope.current === draftScopeKey) setSendError(messageOf(cause)) }) } }] : []),
  ]
  const chooseIntent = () => setIntentMenu({ key: intentMenuKey, version: ++intentMenuVersion.current })
  const send = () => {
    const scope = draftScopeKey
    if (savingDrafts.current.has(scope)) return
    savingDrafts.current.add(scope)
    setSendingScope(scope)
    setSendError('')
    toLatest()
    void (async () => {
      const snapshot = await draft.flush()
      const lease = chatDrafts.lease(snapshot)
      try {
        await workspace.actions.send(id, snapshot.text.trim(), lease.files, { target: targetOption.target, intent: selectedIntent ?? promptIntent(snapshot.text), conversation_id: id, onSaved: (input) => {
          void draft.clear(snapshot).then((cleared) => {
            if (cleared && currentScope.current === scope) setSelectedIntent(null)
            if (cleared && routeActive.current && currentScope.current === scope && live?.isCurrent() && latestLive.current?.conversationId === id && (input.target.kind === 'task' || input.target.kind === 'channel_task')) router.push(`/task/${input.target.task_id}`)
          }).catch((cause) => { if (currentScope.current === scope) setSendError(messageOf(cause)) })
        } })
      } finally { lease.release() }
    })().catch((cause) => { if (currentScope.current === scope) setSendError(messageOf(cause)) }).finally(() => {
      savingDrafts.current.delete(scope)
      setSendingScope((current) => current === scope ? null : current)
    })
  }
  const setupHint = !offline && rows.length && (reach === 'needs_setup' || reach === 'none' || reach === 'offline') ? composerHint({ hasConversation: true, loaded: Boolean(chat), bot: reach }) : ''
  unreadIndex.current = rows.findIndex((row) => row.key === 'unread')
  const mood = offline ? 'asleep' : botMood(bot, workspace, live)
  const statusLine = routine ? '' : chatStatusLine(extras)
  const nextQuestion = routine ? undefined : [...chat?.before.values() ?? [], chat?.rest ?? []].flat().find((question) => questionOpen(question) && live?.answered[question.question_id] === undefined)
  const updates = chatUpdates(chat?.messages ?? [], live?.data?.scheduler?.jobs ?? [], live?.fresh ?? new Set())
  // The bot's hue carries from its face into the send button and starter chips, so each chat reads as that bot's.
  const title = <Link href={`/b/${bot.bot_id}/about`} asChild>
    <Pressable accessibilityRole="link" accessibilityLabel={[bot.name, workspace.primary === bot.conversation_id ? '主 Bot' : '', 'Bot 信息'].filter(Boolean).join('，')}
      className={ios ? 'min-h-11 shrink flex-row items-center gap-1.5 rounded-pill px-2.5 active:bg-surface-muted/50' : '-mx-2 min-h-12 shrink flex-row items-center gap-2 self-start rounded-control px-2 active:bg-surface-muted'} style={{ maxWidth: titleWidth }}>
      <BotFace bot={bot} mood={mood} size={ios ? 24 : 28} />
      <Text numberOfLines={1} className="shrink font-medium">{bot.name}</Text>
      {workspace.primary === bot.conversation_id ? <Icon as={Star} className="size-3.5 fill-muted text-muted" /> : null}
    </Pressable>
  </Link>
  // No background here: the screen paints it, and a painted view would come first natively and hide the list from scrollEdgeEffects.
  return <View className="flex-1">
    {intentMenu?.key === intentMenuKey ? <Modal transparent visible animationType="fade" onRequestClose={() => { intentMenuOwner.current = null; setIntentMenu(null) }}>
      <View className="flex-1 justify-end bg-background/90 p-4" style={{ paddingBottom: insets.bottom + 16 }}>
        <PopoverPanel accessibilityViewIsModal className="max-h-[75%] w-full max-w-3xl self-center gap-2">
          <Text role="heading" numberOfLines={2} className="px-2 py-2 font-medium">{targetOption.label}</Text>
          <ScrollView>{intentOptions.map((option) => <Button key={option.text} variant="ghost" className="justify-start" onPress={() => {
            if (!routeActive.current || currentScope.current !== draftScopeKey || latestLive.current?.generation !== liveGeneration || intentMenuOwner.current !== intentMenu) return
            intentMenuOwner.current = null; setIntentMenu(null); option.action()
          }}><Text className="min-w-0 shrink">{option.text}</Text></Button>)}</ScrollView>
          <Button variant="secondary" onPress={() => { intentMenuOwner.current = null; setIntentMenu(null) }}><Text>取消</Text></Button>
        </PopoverPanel>
      </View>
    </Modal> : null}
    <Stack.Screen options={{
      title: bot.name,
      headerRight: () => <View className="flex-row"><Link href={`/b/${bot.bot_id}/activity`} asChild><Button variant="ghost" iconOnly accessibilityLabel={extras.attention ? `动态，${extras.attention} 项要处理` : '动态'}><Icon as={ListChecks} className="size-5" />{extras.attention ? <View pointerEvents="none" className="absolute right-2 top-2 size-2 rounded-pill bg-warning" /> : null}</Button></Link><Link href={{ pathname: '/search', params: { bot: bot.bot_id } }} asChild><Button variant="ghost" iconOnly accessibilityLabel="搜索聊天记录"><Icon as={Search} className="size-5" /></Button></Link><Link href={`/b/${bot.bot_id}/about`} asChild><Button variant="ghost" iconOnly accessibilityLabel={`${bot.name} 设置`}><Icon as={Settings} className="size-5" /></Button></Link></View>,
      // iOS 26 measures the edge blur on the un-inverted list and covers every message once the chat outgrows the screen.
      scrollEdgeEffects: { top: 'hidden', bottom: 'hidden', left: 'hidden', right: 'hidden' },
      headerTransparent: ios,
      ...ios ? {
        headerBlurEffect: 'none',
        headerStyle: { backgroundColor: typeof background === 'string' ? background : undefined },
      } : {},
      headerTitle: () => <ScreenFaces>{title}</ScreenFaces>,
    }} />
    <ScreenFaces><View className="flex-1">
    {/* Always mounted, so the first message sent with the keyboard open lands in the keyboard-aware list. */}
    {routine ? <RoutineRecord workspace={live} botId={bot.bot_id} conversationId={id} trigger={routine} notificationId={notification ?? null} /> : <FlatList
      key={id}
      ref={list}
      data={rows}
      inverted
      onViewableItemsChanged={viewability.current}
      viewabilityConfig={{ viewAreaCoveragePercentThreshold: 20, minimumViewTime: 600 }}
      onEndReached={loadOlder}
      onEndReachedThreshold={0.25}
      onScrollBeginDrag={() => { restoration.current.done = true }}
      maintainVisibleContentPosition={scrollButton.id === id && scrollButton.visible ? { minIndexForVisible: 0 } : undefined}
      windowSize={7}
      maxToRenderPerBatch={8}
      updateCellsBatchingPeriod={32}
      keyExtractor={(row) => row.key}
      onScrollToIndexFailed={({ index, averageItemLength }) => {
        if (scrollInset.current.id !== id) return
        list.current?.scrollToOffset({ offset: averageItemLength * index, animated: false })
        setTimeout(() => toUnread(index), 100)
      }}
      onScroll={({ nativeEvent }) => {
        if (scrollInset.current.id !== id) return
        const visible = awayFromLatest(nativeEvent, scrollInset.current, ios)
        away.current = visible
        setScrollButton((current) => current.id === id && current.visible === visible ? current : { id, visible })
      }}
      scrollEventThrottle={16}
      keyboardDismissMode="interactive"
      keyboardShouldPersistTaps="handled"
      renderScrollComponent={(props) => <KeyboardChatScrollView {...props} inverted offset={insets.bottom} keyboardLiftBehavior="whenAtEnd"
        extraContentPadding={ios ? dockHeight : undefined}
        onContentInsetChange={(inset) => { if (scrollInset.current.id === id) scrollInset.current = { id, top: inset.top, bottom: inset.bottom } }} />}
      contentContainerClassName={ios ? 'w-full max-w-3xl self-center px-4 pt-2 pb-4' : 'w-full max-w-3xl self-center px-4 py-6'}
      // The inverted list's footer is its top: the oldest message starts below the transparent bar.
      ListFooterComponent={<View style={{ height: header }} />}
      ItemSeparatorComponent={Separator}
      renderItem={renderItem}
    />}
    {routine || rows.length ? null : <Animated.View pointerEvents="box-none" style={[StyleSheet.absoluteFill, { paddingTop: header }, above]}>
      <View pointerEvents="box-none" className="flex-1 items-center justify-center gap-4 px-4">
        <BotFace bot={bot} mood={mood} size={96} />
        <View className="items-center gap-1">
          <Text role="heading" className="text-center text-lg font-medium">{bot.name}</Text>
          {workspace.primary === bot.conversation_id ? <View className="flex-row items-center gap-1">
            <Icon as={Star} className="size-3.5 fill-muted text-muted" /><Text className="text-sm text-muted">主 Bot</Text>
          </View> : null}
        </View>
        {!chat ? <View accessibilityLiveRegion="polite" className="items-center gap-2"><Text className="text-center text-muted">{live?.conversationLoading ? '正在读取聊天记录…' : live?.conversationError ? '没读到聊天记录' : offline ? '连上后可读取' : '正在连接聊天记录…'}</Text>{live && !live.conversationLoading ? <Button variant="secondary" onPress={() => void live.refreshConversation().catch((cause) => setSendError(messageOf(cause)))}><Text>重新读取</Text></Button> : null}</View> : botCanSend(reach) ? <Starters draft={draft} project={Boolean(bot.project)} canRun={botCanSend(reach)} /> : starting ? <View role="status" className="flex-row items-center gap-1.5"><Icon as={LoaderCircle} className="text-muted" /><Text className="text-muted">Bot 正在启动</Text></View> : <>
          <View className="flex-row items-center gap-1.5"><Icon as={MessageSquareOff} className="text-muted" /><Text className="text-muted">Bot 还不能回复</Text></View>
          {reach === 'offline' || canSetUp ? <Link href={setupHref} asChild>
            <Button><Icon as={Bot} /><Text>{reach === 'offline' ? '查看设备' : '设置运行 Bot 的设备'}</Text></Button>
          </Link> : <View className="flex-row items-center gap-1.5"><Icon as={Crown} className="size-4 text-muted" /><Text className="text-sm text-muted">{setUpOnManager(workspace.members.filter((member) => member.admin).map((member) => member.name))}</Text></View>}
        </>}
      </View>
    </Animated.View>}
    <RecoveryAlert top={header} />
    </View></ScreenFaces>
    <KeyboardStickyView pointerEvents={ios ? 'box-none' : undefined} style={ios ? { position: 'absolute', bottom: 0, left: 0, right: 0 } : undefined} offset={{ closed: 0, opened: insets.bottom }}>
      <View pointerEvents={ios ? 'box-none' : undefined} className="bg-background" style={{ paddingBottom: insets.bottom }} onLayout={ios ? (event) => { dockHeight.value = event.nativeEvent.layout.height } : undefined}>
        {scrollButton.id === id && scrollButton.visible ? <View pointerEvents="box-none" className={ios ? 'absolute -top-13 w-full max-w-3xl self-center items-end px-4' : 'absolute -top-14 w-full max-w-3xl self-center items-end px-4'}>
          <Button variant="secondary" iconOnly className="rounded-pill bg-surface" accessibilityLabel="回到底部" onPress={toLatest}><Icon as={ArrowDown} /></Button>
        </View> : null}
        {location.kind !== 'found' ? <View role="status" className="w-full max-w-3xl gap-2 self-center px-4 py-2"><Text className="text-sm text-muted">{location.text}</Text>{location.kind === 'failed' ? <Button variant="secondary" onPress={location.retry}><Text>重新定位原记录</Text></Button> : null}</View> : null}
        {nextQuestion ? <View className="px-4"><Button variant="secondary" accessibilityLabel={`现在处理：${questionParts(nextQuestion.text).ask}`} onPress={() => {
          const index = rows.findIndex((row) => row.key === `q:${nextQuestion.question_id}`)
          if (index >= 0) toUnread(index)
        }}><Text numberOfLines={2} className="min-w-0 shrink">{offline ? '最近记录 · ' : ''}{questionParts(nextQuestion.text).ask}</Text><Icon as={ArrowDown} /></Button></View> : !routine && updates.length ? <View accessibilityLabel="自上次查看以来" className="w-full max-w-3xl gap-1 self-center px-4"><Text numberOfLines={2} className="text-sm text-muted">{offline ? '最近记录' : '自上次查看以来'} · {updates.at(-1)?.state} · {updates.at(-1)?.text}</Text></View> : null}
        {statusLine ? <View className="bg-background px-2"><Link href={`/b/${bot.bot_id}/activity`} asChild>
          <Pressable accessibilityRole="link" accessibilityHint="打开动态" className={`${touchRow} w-full max-w-3xl flex-row items-center gap-1.5 self-center rounded-control px-2 active:bg-surface-muted`}>
            <Icon as={ListChecks} className={extras.attention ? 'size-4 text-warning' : 'size-4 text-muted'} />
            <Text numberOfLines={1} className="min-w-0 flex-1 text-sm text-muted">{statusLine}</Text>
            <Icon as={ChevronRight} className="size-4 text-muted" />
          </Pressable>
        </Link></View> : null}
        <DraftComposer draft={draft} attachmentScope={JSON.stringify([workspace.networkId, id, inputTargetKey(inputTarget)])}
          blocked={blocked} sending={sendingScope === draftScopeKey} onSend={send} onStop={(inputTarget.kind === 'bot_main' ? chat?.stoppable : targetBusy) ? () => void live?.stopInputTarget({ target: targetOption.target }).catch((cause) => setSendError(messageOf(cause))) : undefined}
          selectedIntent={selectedIntent} targetBusy={targetBusy} onChooseIntent={chooseIntent}
        >
        <Text numberOfLines={1} className="text-sm text-muted">{targetOption.label}</Text>
        {live?.conversationError ? <View className="gap-1"><ErrorText error={live.conversationError} /><Button variant="ghost" size="sm" onPress={() => void live.refreshConversation().catch((cause) => setSendError(messageOf(cause)))}><Text>重新读取聊天</Text></Button></View> : null}
        {offline || setupHint || sendError || inputTarget.kind !== 'bot_main' ? <>
          {offline ? <OfflineBar chat className="self-center" /> : setupHint ? <View role="status" className="flex-row flex-wrap items-center gap-x-2 px-2">
            <Text className="text-sm text-muted">{setupHint}</Text>
            {reach === 'offline' || canSetUp ? <Link href={setupHref} asChild><Button variant="ghost" size="sm"><Text>{reach === 'offline' ? '查看设备' : '去设置'}</Text></Button></Link>
              : <Text className="text-sm text-muted">{setUpOnManager(workspace.members.filter((member) => member.admin).map((member) => member.name))}</Text>}
          </View> : null}
          {sendError ? <ErrorText error={sendError} /> : null}
          {inputTarget.kind !== "bot_main" ? <View className="flex-row items-center gap-2 px-2">
            <Text numberOfLines={1} className="min-w-0 flex-1 text-sm text-muted">{targetBusy ? '补充给' : '发送给'} · {targetOption.label}</Text>
            <Button variant="ghost" size="sm" onPress={() => setSelectedTarget('main')}><Text>改回主聊天</Text></Button>
          </View> : null}
        </> : null}
        </DraftComposer>
      </View>
    </KeyboardStickyView>
  </View>
}


function RoutineRecord({ workspace, botId, conversationId, trigger, notificationId }: { workspace: ReturnType<typeof useLive>; botId: string; conversationId: string; trigger: string; notificationId: string | null }) {
  const record = useNotificationRecord({ workspace, conversationId, target: { kind: 'routine', bot_id: botId, trigger }, notificationId })
  const bot = workspace?.bots.find((bot) => bot.bot_id === botId)
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-3 p-4 pb-32" accessibilityLabel="原例行记录">
    <Text role="heading" className="text-sm font-medium">{scheduleName(trigger, workspace?.data?.bot_settings, workspace?.data?.changes)}</Text>
    {record.kind === 'record' ? record.content.kind === 'question' ? <QuestionCard question={record.content.question} /> : bot ? <Message message={record.content.message} bot={bot} offline={!workspace?.connectionOnline} attachmentScope={JSON.stringify([conversationId, trigger])} /> : null : <Text role="status" className="text-sm text-muted">{record.text}</Text>}
    {record.kind === 'failed' ? <Button variant="secondary" onPress={record.retry}><Text>重新读取原记录</Text></Button> : null}
  </ScrollView>
}
