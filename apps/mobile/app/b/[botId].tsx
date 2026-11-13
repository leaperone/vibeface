import { historySections } from '@vibeface/client/workspace/model.ts'
import { HistoryControls } from '@/features/chat/history-controls'
import { Link, Stack, router, useFocusEffect, useLocalSearchParams } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { ArrowDown, Bot, Crown, LoaderCircle, MessageSquareOff, Settings, Star } from 'lucide-react-native'
import { useCallback, useEffect, useRef, useState } from 'react'
import { FlatList, Platform, Pressable, StyleSheet, View, type NativeScrollEvent } from 'react-native'
import { KeyboardChatScrollView, KeyboardStickyView, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Animated, { useAnimatedStyle, useSharedValue } from 'react-native-reanimated'
import { useSafeAreaFrame, useSafeAreaInsets } from 'react-native-safe-area-context'
import { useCSSVariable, useUniwind, withUniwind } from 'uniwind'
import { faceOf } from '@vibeface/client/avatar/face.ts'
import { awaitsDevice, botReach, botCanSend, composerHint, inputTargetKey, promptIntent, starters, reachDevice } from '@vibeface/client/chat/protocol.ts'
import { messageTimes } from '@vibeface/client/chat/timeline.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { setUpOnManager } from '@vibeface/client/identity/copy.ts'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { ErrorText } from '@/components/action'
import { outcomeUnknown } from '@vibeface/client/lib/errors.ts'
import { botCopy, scheduleName } from '@vibeface/client/workspace/bot-settings.ts'
import { unreachable } from '@vibeface/client/workspace/home.ts'
import { botMood, BotFace, ScreenFaces, type FaceBot } from '@/components/bot-face'
import { OfflineBar } from '@/components/connection-pill'
import { botTint, EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import type { Attachment, ChatMessage, Conversation } from '@/data/types'
import type { Question } from '@vibeface/client/workspace/types.ts'
import { Composer } from '@/features/chat/composer'
import { useChatChrome } from '@/features/chat/chrome'
import { Message } from '@/features/chat/message'
import { QuestionCard } from '@/features/chat/question-card'
import { useChatExtras, type Row } from '@/features/chat/work'
import { RecoveryAlert } from '@/features/recovery/recovery'
import { useWide } from '@/lib/layout'
import { fadeIn } from '@/lib/motion'
import { networkKey, storageOf } from '@vibeface/client/workspace/storage.ts'
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
 * Questions sit right before the first message written after them, 新消息 above what the badge counted, tasks and changes right after the message that caused
 * them and what waits on the person last, as on the web; the newest entry is first for the inverted list.
 */
function entries(chat: Conversation, extras: ReturnType<typeof useChatExtras>, bot: FaceBot, offline: boolean): Entry[] {
  const rows: Entry[] = [...extras.first]
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
    rows.push(...(extras.after.get(message.message_id) ?? []).map((row) => ({ ...row, attached: true })))
  }
  rows.push(...chat.rest.map(question))
  const unreadAt = rows.findIndex((row) => ('message' in row ? row.message.message_id : row.key) === chat.unread)
  if (unreadAt >= 0) rows.splice(unreadAt, 0, { key: 'unread', node: <View className="flex-row items-center gap-3"><View className="h-px flex-1 bg-primary/40" /><Text className="text-meta text-primary">新消息</Text><View className="h-px flex-1 bg-primary/40" /></View> })
  rows.push(...extras.rest)
  if (chat.replying && !chat.messages.some((message) => message.state === 'streaming')) rows.push({
    key: 'activity',
    node: <Fade entering={fadeIn} accessibilityRole="progressbar" className="min-h-7 flex-row items-center gap-2"><BotFace bot={bot} mood={offline ? 'asleep' : 'thinking'} size={24} /><Text className="text-meta text-muted-foreground">{chat.status ?? '思考中'}</Text></Fade>,
  })
  if (extras.cards.length) rows.push({ key: 'waiting-on-you', node: <View className="gap-3">{extras.cards}</View> })
  return rows.reverse()
}

export default function ChatScreen() {
  const { botId, routine } = useLocalSearchParams<{ botId: string; routine?: string }>()
  const workspace = useWorkspace()
  const device = useDevice()
  const insets = useSafeAreaInsets()
  // Only iOS draws the bar over the list; Android's stays opaque above it.
  const ios = Platform.OS === 'ios'
  const barHeight = useHeaderHeight()
  const header = ios ? barHeight : 0
  // iOS sizes a custom title against the whole bar, back button included, so a long name would run under it; the screen's
  // width less the sidebar and a back button's room on each side keeps it centred and truncated. Android's title slot
  // already ends where the bar's buttons begin.
  const frame = useSafeAreaFrame().width
  const wide = useWide()
  const titleWidth = ios ? (wide ? frame - 320 : frame) - 120 : undefined
  const { theme } = useUniwind()
  const chrome = useChatChrome()
  const background = useCSSVariable('--background')
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const id = bot?.conversation_id
  const chat = id ? workspace.conversations[id] : undefined
  const live = useLive()
  const latestWorkspace = useRef(workspace)
  latestWorkspace.current = workspace
  const draftStorage = storageOf(mobilePlatform.storage)
  const draftKey = networkKey('bot-drafts', device.ready?.principal.network_id ?? '')
  const [drafts, setDrafts] = useState<Record<string, { text: string; files: Attachment[] }>>(() => { const stored = draftStorage.readStorage<unknown>(draftKey, {}); return stored && typeof stored === 'object' && !Array.isArray(stored) ? Object.fromEntries(Object.entries(stored).filter((entry): entry is [string, string] => typeof entry[1] === 'string').map(([id, text]) => [id, { text, files: [] }])) : {} })
  const [selectedTarget, setSelectedTarget] = useState(routine ? `routine:${routine}` : 'main')
  const [sendError, setSendError] = useState('')
  useEffect(() => { setSelectedTarget(routine ? `routine:${routine}` : 'main') }, [id, routine])
  const draft = (id && drafts[id]) || { text: '', files: [] }
  const draftRef = useRef({ value: draft, target: selectedTarget, conversationId: id })
  if (draftRef.current.value !== draft || draftRef.current.target !== selectedTarget || draftRef.current.conversationId !== id) draftRef.current = { value: draft, target: selectedTarget, conversationId: id }
  const savingDrafts = useRef(new Set<typeof draftRef.current>())
  const setDraft = (next: Partial<typeof draft>) => { if (id) setDrafts((current) => {
    const present = current[id] ?? { text: '', files: [] }, changed = { ...present, ...next }
    if (changed.text === present.text && changed.files === present.files) return current
    const updated = { ...current, [id]: changed }
    draftStorage.writeStorage(draftKey, Object.fromEntries(Object.entries(updated).map(([key, value]) => [key, value.text])))
    return updated
  }) }
  const routeActive = useRef(false)
  useFocusEffect(useCallback(() => { routeActive.current = true; const close = id ? latestWorkspace.current.actions.open(id) : undefined; return () => { routeActive.current = false; close?.() } }, [id, live?.generation]))
  const extras = useChatExtras(id ?? '', botId)
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
  const toLatest = () => {
    if (scrollInset.current.id !== id) return
    setScrollButton({ id, visible: false })
    list.current?.scrollToOffset({ offset: ios ? -scrollInset.current.top : 0, animated: true })
  }
  // Opening a chat with 新消息 brings it to the top of the screen, the unread under it.
  const unreadIndex = useRef(-1)
  const toUnread = (index: number) => { if (scrollInset.current.id === id) list.current?.scrollToIndex({ index, viewPosition: 1, animated: false }) }
  useEffect(() => {
    const index = unreadIndex.current
    if (index >= 0) requestAnimationFrame(() => toUnread(index))
  }, [id, Boolean(chat?.unread)])
  // The empty state stays centred in the space above the composer instead of riding up with the list.
  const above = useAnimatedStyle(() => ({ paddingBottom: ios
    ? dockHeight.value + Math.max(0, Math.abs(keyboard.height.value) - insets.bottom * keyboard.progress.value)
    : Math.max(0, Math.abs(keyboard.height.value) - insets.bottom) }))

  if (!bot || !id) return <EmptyState icon={Bot} title={botCopy.notFound} className="flex-1 justify-center" />
  const offline = unreachable(device.link)
  const blocked = chat?.blocked ?? (chat ? null : '尚未完成首读')
  const routineTrigger = selectedTarget.startsWith('routine:') ? selectedTarget.slice(8) : null
  const targetOption = routineTrigger ? { label: `例行任务 · ${scheduleName(routineTrigger, live?.data?.bot_settings, live?.data?.changes)}`, target: { kind: 'routine' as const, bot_id: bot.bot_id, trigger: routineTrigger } } : { label: `${bot.name} · 主聊天`, target: { kind: 'bot_main' as const, bot_id: bot.bot_id } }
  const inputTarget = targetOption.target
  const targetBusy = inputTarget.kind === 'routine' ? Boolean(live?.data?.scopeJobs.some((job) => job.trigger === inputTarget.trigger && ['queued', 'steering', 'claimed', 'running'].includes(job.state))) : chat?.replying
  const send = () => {
    const submittedDraft = draftRef.current
    if (savingDrafts.current.has(submittedDraft)) return
    savingDrafts.current.add(submittedDraft)
    const { text, files } = submittedDraft.value
    setSendError('')
    toLatest()
    workspace.actions.send(id, text.trim(), files, { target: targetOption.target, intent: promptIntent(text), conversation_id: id, onSaved: (input) => {
      if (!live?.isCurrent()) return
      setDrafts((current) => {
        const present = current[id]
        if (present !== submittedDraft.value || draftRef.current.conversationId === id && draftRef.current !== submittedDraft) return current
        const updated = { ...current, [id]: { text: '', files: [] } }
        draftStorage.writeStorage(draftKey, Object.fromEntries(Object.entries(updated).map(([key, value]) => [key, value.text])))
        return updated
      })
      if (!routeActive.current || !live?.isCurrent() || latestLive.current?.conversationId !== id || draftRef.current !== submittedDraft) return; if (input.target.kind === "task" || input.target.kind === "channel_task") router.push(`/task/${input.target.task_id}`) } }).catch((cause) => { setSendError(messageOf(cause)) }).finally(() => { savingDrafts.current.delete(submittedDraft) })
  }
  const rows = chat ? entries(chat, extras, bot, offline) : []
  const history = live?.conversationId === id ? live.data?.history : undefined
  if (history && live && historySections.some((section) => !history[section].complete)) rows.push({ key: 'history-controls', node: <HistoryControls key={`${live.generation}:${id}`} workspace={live} /> })
  const setupHint = !offline && rows.length && (reach === 'needs_setup' || reach === 'none' || reach === 'offline') ? composerHint({ hasConversation: true, loaded: Boolean(chat), bot: reach }) : ''
  // 重试 on the newest reply once it failed or was stopped, as on the web.
  const newest = chat?.messages.at(-1)
  const asked = newest && ['failed', 'stopped'].includes(newest.status ?? '') ? newest.in_reply_to : undefined
  const unknown = live?.data?.scheduler?.jobs.some((job) => job.message_id === asked && outcomeUnknown(job.failure?.kind, job.failure?.detail))
  const retry = asked && live && !blocked && !unknown ? () => live.retryReply(asked) : undefined
  unreadIndex.current = rows.findIndex((row) => row.key === 'unread')
  const mood = offline ? 'asleep' : botMood(bot, workspace, live)
  // The bot's hue carries from its face into the send button and starter chips, so each chat reads as that bot's.
  const tint = botTint(faceOf(bot.bot_id, bot.avatar).hue, theme === 'dark')
  const title = <Link href={`/b/${bot.bot_id}/about`} asChild>
    <Pressable accessibilityRole="link" accessibilityLabel={[bot.name, workspace.primary === bot.conversation_id ? '主 Bot' : '', 'Bot 信息'].filter(Boolean).join('，')}
      className={ios ? 'min-h-11 shrink flex-row items-center gap-1.5 rounded-full px-2.5 active:bg-muted/50' : '-mx-2 min-h-11 shrink flex-row items-center gap-2 self-start rounded-lg px-2 active:bg-muted'} style={{ maxWidth: titleWidth }}>
      <BotFace bot={bot} mood={mood} size={ios ? 24 : 28} />
      <Text numberOfLines={1} className="shrink font-semibold">{bot.name}</Text>
      {workspace.primary === bot.conversation_id ? <Icon as={Star} className="size-3.5 fill-muted-foreground text-muted-foreground" /> : null}
    </Pressable>
  </Link>
  // No background here: the screen paints it, and a painted view would come first natively and hide the list from scrollEdgeEffects.
  return <View className="flex-1">
    <Stack.Screen options={{
      title: bot.name,
      headerRight: () => <Link href={`/b/${bot.bot_id}/about`} asChild><Button variant="ghost" size="icon" accessibilityLabel={`${bot.name} 设置`}><Icon as={Settings} className="size-5" /></Button></Link>,
      // iOS 26 measures the edge blur on the un-inverted list and covers every message once the chat outgrows the screen.
      scrollEdgeEffects: { top: 'hidden', bottom: 'hidden', left: 'hidden', right: 'hidden' },
      headerTransparent: ios,
      ...ios ? {
        headerBlurEffect: chrome.kind === 'glass' ? chrome.colorScheme === 'dark' ? 'systemUltraThinMaterialDark' : 'systemUltraThinMaterialLight' : 'none',
        headerStyle: { backgroundColor: chrome.kind === 'glass' ? 'transparent' : typeof background === 'string' ? background : undefined },
      } : {},
      headerTitle: () => <ScreenFaces>{title}</ScreenFaces>,
    }} />
    <ScreenFaces><View className="flex-1">
    {/* Always mounted, so the first message sent with the keyboard open lands in the keyboard-aware list. */}
    <FlatList
      key={id}
      ref={list}
      data={rows}
      inverted
      maintainVisibleContentPosition={scrollButton.id === id && scrollButton.visible ? { minIndexForVisible: 0 } : undefined}
      keyExtractor={(row) => row.key}
      onScrollToIndexFailed={({ index, averageItemLength }) => {
        if (scrollInset.current.id !== id) return
        list.current?.scrollToOffset({ offset: averageItemLength * index, animated: false })
        setTimeout(() => toUnread(index), 100)
      }}
      onScroll={({ nativeEvent }) => {
        if (scrollInset.current.id !== id) return
        const visible = awayFromLatest(nativeEvent, scrollInset.current, ios)
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
      // Inverted, so the leading item is the lower one: a card caused by the message above it stays close to it.
      ItemSeparatorComponent={({ leadingItem }: { leadingItem: Entry }) => <View className={'grouped' in leadingItem && leadingItem.grouped ? 'h-2' : 'attached' in leadingItem && leadingItem.attached ? 'h-3' : 'h-6'} />}
      renderItem={({ item }) => 'message' in item ? <Message message={item.message} bot={bot} time={item.time} offline={offline} retry={item.message === newest ? retry : undefined} /> : <>{item.node}</>}
    />
    {rows.length ? null : <Animated.View pointerEvents="box-none" style={[StyleSheet.absoluteFill, { paddingTop: header }, above]}>
      <View pointerEvents="box-none" className="flex-1 items-center justify-center gap-4 px-4">
        <BotFace bot={bot} mood={mood} size={96} halo={tint.glow} />
        <View className="items-center gap-1">
          <Text role="heading" className="text-center text-heading font-semibold">{bot.name}</Text>
          {workspace.primary === bot.conversation_id ? <View className="flex-row items-center gap-1">
            <Icon as={Star} className="size-3.5 fill-muted-foreground text-muted-foreground" /><Text className="text-meta text-muted-foreground">主 Bot</Text>
          </View> : null}
        </View>
        {botCanSend(reach) ? <View pointerEvents="box-none" className="flex-row flex-wrap justify-center gap-2">
          {starters[bot.project ? 'project' : 'general'].map((example) => <Button key={example} variant="outline" className="rounded-full" style={{ borderColor: tint.edge }} onPress={() => setDraft({ text: example })}><Text className="text-muted-foreground">{example}</Text></Button>)}
        </View> : starting ? <View role="status" className="flex-row items-center gap-1.5"><Icon as={LoaderCircle} className="text-muted-foreground" /><Text className="text-muted-foreground">Bot 正在启动</Text></View> : <>
          <View className="flex-row items-center gap-1.5"><Icon as={MessageSquareOff} className="text-muted-foreground" /><Text className="text-muted-foreground">Bot 还不能回复</Text></View>
          {reach === 'offline' || canSetUp ? <Link href={setupHref} asChild>
            <Button><Icon as={Bot} /><Text>{reach === 'offline' ? '查看设备' : '设置运行 Bot 的设备'}</Text></Button>
          </Link> : <View className="flex-row items-center gap-1.5"><Icon as={Crown} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{setUpOnManager(workspace.members.filter((member) => member.admin).map((member) => member.name))}</Text></View>}
        </>}
      </View>
    </Animated.View>}
    <RecoveryAlert top={header} />
    </View></ScreenFaces>
    <KeyboardStickyView pointerEvents={ios ? 'box-none' : undefined} style={ios ? { position: 'absolute', bottom: 0, left: 0, right: 0 } : undefined} offset={{ closed: 0, opened: insets.bottom }}>
      <View pointerEvents={ios ? 'box-none' : undefined} style={{ paddingBottom: insets.bottom }} onLayout={ios ? (event) => { dockHeight.value = event.nativeEvent.layout.height } : undefined}>
        {scrollButton.id === id && scrollButton.visible ? <View pointerEvents="box-none" className={ios ? 'absolute -top-13 w-full max-w-3xl self-center items-end px-4' : 'absolute -top-14 w-full max-w-3xl self-center items-end px-4'}>
          <Button variant="outline" size="icon" className="rounded-full bg-card shadow-sm" accessibilityLabel="回到底部" onPress={toLatest}><Icon as={ArrowDown} /></Button>
        </View> : null}
        <Composer chrome={chrome} value={draft.text} onChange={(text) => setDraft({ text })} files={draft.files} onFiles={(files) => setDraft({ files })} attachmentScope={JSON.stringify([device.ready?.principal.network_id ?? '', id, inputTargetKey(inputTarget)])}
          blocked={blocked} sending={false} onSend={send} onStop={(inputTarget.kind === 'bot_main' ? chat?.stoppable : targetBusy) ? () => void live?.stopInputTarget({ target: targetOption.target }).catch((cause) => setSendError(messageOf(cause))) : undefined} tint={tint}
          sendLabel={targetBusy ? '补充当前工作' : '发送'}
        >{offline || setupHint || sendError || inputTarget.kind !== 'bot_main' ? <>
          {offline ? <OfflineBar chat className="self-center" /> : setupHint ? <View role="status" className="flex-row flex-wrap items-center gap-x-2 px-2">
            <Text className="text-meta text-muted-foreground">{setupHint}</Text>
            {reach === 'offline' || canSetUp ? <Link href={setupHref} asChild><Button variant="ghost" size="sm" className="min-h-12"><Text>{reach === 'offline' ? '查看设备' : '去设置'}</Text></Button></Link>
              : <Text className="text-meta text-muted-foreground">{setUpOnManager(workspace.members.filter((member) => member.admin).map((member) => member.name))}</Text>}
          </View> : null}
          {sendError ? <ErrorText error={sendError} /> : null}
          {inputTarget.kind !== "bot_main" ? <View className="flex-row items-center gap-2 px-2">
            <Text numberOfLines={1} className="min-w-0 flex-1 text-meta text-muted-foreground">{targetBusy ? '补充给' : '发送给'} · {targetOption.label}</Text>
            <Button variant="ghost" size="sm" className="min-h-12" onPress={() => setSelectedTarget('main')}><Text>改回主聊天</Text></Button>
          </View> : null}
        </> : null}
        </Composer>
      </View>
    </KeyboardStickyView>
  </View>
}
