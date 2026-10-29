import { Link, Stack, useFocusEffect, useLocalSearchParams } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { Bot, Crown, LoaderCircle, MessageSquareOff, Star } from 'lucide-react-native'
import { useCallback, useEffect, useRef, useState } from 'react'
import { FlatList, Platform, Pressable, StyleSheet, View } from 'react-native'
import { KeyboardChatScrollView, KeyboardStickyView, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Animated, { useAnimatedStyle } from 'react-native-reanimated'
import { useSafeAreaFrame, useSafeAreaInsets } from 'react-native-safe-area-context'
import { useUniwind, withUniwind } from 'uniwind'
import { faceOf } from '@vibeface/client/avatar/face.ts'
import { awaitsDevice, botReach, starters } from '@vibeface/client/chat/protocol.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { botMood, BotFace, ScreenFaces } from '@/components/bot-face'
import { botTint, EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import type { Attachment, ChatMessage, Conversation } from '@/data/types'
import { Composer } from '@/features/chat/composer'
import { Message } from '@/features/chat/message'
import { QuestionCard } from '@/features/chat/question-card'
import { useChatExtras, type Row } from '@/features/chat/work'
import { RecoveryAlert } from '@/features/recovery/recovery'
import { useWide } from '@/lib/layout'
import { fadeIn } from '@/lib/motion'

/** One row of the chat: a message, or a question or status line between them. */
type Entry = { key: string; message: ChatMessage; last: boolean; grouped: boolean } | (Row & { attached?: boolean })

const Fade = withUniwind(Animated.View)

/**
 * Questions sit right before the first message written after them, tasks and changes right after the message that caused
 * them and what waits on the person last, as on the web; the newest entry is first for the inverted list.
 */
function entries(chat: Conversation, extras: ReturnType<typeof useChatExtras>, botId: string, offline: boolean): Entry[] {
  const rows: Entry[] = [...extras.first]
  const questions = [...chat.questions].sort((a, b) => a.created_at_ms - b.created_at_ms)
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
  messages.forEach((message, index) => {
    while (questions.length && questions[0].created_at_ms < message.created_at_ms) {
      const question = questions.shift()!
      rows.push({ key: `q:${question.question_id}`, node: <QuestionCard question={question} /> })
    }
    const next = messages[index + 1]
    // The time closes a burst: the newest message, or one followed by a quiet spell, not every turn.
    const previous = rows.at(-1)
    const grouped = previous !== undefined && 'message' in previous && !previous.last && previous.message.role === message.role
    rows.push({ key: keyOf(message), message, last: !next || next.created_at_ms - message.created_at_ms > 5 * 60_000, grouped })
    rows.push(...(extras.after.get(message.message_id) ?? []).map((row) => ({ ...row, attached: true })))
  })
  for (const question of questions) rows.push({ key: `q:${question.question_id}`, node: <QuestionCard question={question} /> })
  rows.push(...extras.rest)
  if (chat.replying && !chat.messages.some((message) => message.state === 'streaming')) rows.push({
    key: 'activity',
    node: <Fade entering={fadeIn} accessibilityRole="progressbar" className="min-h-7 flex-row items-center gap-2"><BotFace botId={botId} mood={offline ? 'asleep' : 'thinking'} size={24} /><Text className="text-meta text-muted-foreground">{chat.status ?? '思考中'}</Text></Fade>,
  })
  if (extras.cards.length) rows.push({ key: 'waiting-on-you', node: <View className="gap-3">{extras.cards}</View> })
  return rows.reverse()
}

export default function ChatScreen() {
  const { botId } = useLocalSearchParams<{ botId: string }>()
  const workspace = useWorkspace()
  const insets = useSafeAreaInsets()
  // Only iOS draws the bar over the list; Android's stays opaque above it.
  const ios = Platform.OS === 'ios'
  const barHeight = useHeaderHeight()
  const header = ios ? barHeight : 0
  // A custom title is sized against the whole bar, back button included, so a long name would run under it; the screen's
  // width less the sidebar and a back button's room on each side keeps it centred and truncated.
  const frame = useSafeAreaFrame().width
  const titleWidth = (useWide() ? frame - 320 : frame) - 120
  const { theme } = useUniwind()
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const id = bot?.conversation_id
  const chat = id ? workspace.conversations[id] : undefined
  const [drafts, setDrafts] = useState<Record<string, { text: string; files: Attachment[] }>>({})
  const [launching, setLaunching] = useState(false)
  const draft = (id && drafts[id]) || { text: '', files: [] }
  const setDraft = (next: Partial<typeof draft>) => { if (id) setDrafts((current) => ({ ...current, [id]: { ...draft, ...next } })) }
  useEffect(() => id ? workspace.actions.open(id) : undefined, [id])
  const extras = useChatExtras(id ?? '', botId)
  const live = useLive()
  const device = useDevice()
  // As on the web: until a device can reply, the empty chat says so and points a manager at the AI settings instead of offering starters.
  const reach = live ? botReach(live.nodes) : 'ready'
  const canSetUp = device.ready?.principal.can_manage === true
  // Nothing pushes a device's bot readiness, so while a device that could reply is not ready and this chat is on screen, read it again every 10s, or every 2s while it starts.
  const latestLive = useRef(live)
  latestLive.current = live
  const awaiting = live ? awaitsDevice(reach) : false, starting = reach === 'starting'
  useFocusEffect(useCallback(() => {
    if (!awaiting) return
    const read = () => { if (latestLive.current && !latestLive.current.refreshing) void latestLive.current.refresh().catch(() => {}) }
    read()
    const timer = setInterval(read, starting ? 2000 : 10000)
    return () => clearInterval(timer)
  }, [awaiting, starting]))
  const keyboard = useReanimatedKeyboardAnimation()
  const list = useRef<FlatList<ReturnType<typeof entries>[number]>>(null)
  // A failed send hands the draft back, but its outbox copy may still go out on its own; once it lands the draft is
  // cleared if untouched, so a second tap never posts it twice. Until then a resend reuses the pending message.
  const unsettled = useRef<{ text: string; seen: number } | null>(null)
  const sent = (text: string) => chat?.messages.filter((message) => message.role === 'user' && message.state !== 'sending' && message.text === text).length ?? 0
  const landed = unsettled.current !== null && sent(unsettled.current.text) > unsettled.current.seen
  useEffect(() => {
    if (!landed || !unsettled.current) return
    if (draft.text.trim() === unsettled.current.text) setDraft({ text: '', files: [] })
    unsettled.current = null
  }, [landed])
  // The empty state stays centred in the space above the composer instead of riding up with the list.
  const above = useAnimatedStyle(() => ({ paddingBottom: Math.max(0, Math.abs(keyboard.height.value) - insets.bottom) }))

  if (!bot || !id) return <EmptyState icon={Bot} title="没有这个 Bot" className="flex-1 justify-center" />
  const offline = workspace.connection.state === 'offline'
  const blocked = offline ? '离线，恢复后可发送' : chat?.blocked ?? (chat ? null : '稍等')
  const send = () => {
    const { text, files } = draft
    setDraft({ text: '', files: [] })
    // Held from the tap itself: the shared sending flag lands a frame or two after the draft clears.
    setLaunching(true)
    // iOS makes room for the keyboard with an inset, so the newest message rests at a negative offset, not at 0.
    list.current?.scrollToOffset({ offset: ios ? -Math.max(0, Math.abs(keyboard.height.value) - insets.bottom) : 0, animated: true })
    workspace.actions.send(id, text.trim(), files).catch(() => { unsettled.current = { text: text.trim(), seen: sent(text.trim()) }; setDraft({ text, files }) }).finally(() => setLaunching(false))
  }
  const rows = chat ? entries(chat, extras, bot.bot_id, offline) : []
  const mood = offline ? 'asleep' : botMood(bot, workspace, live)
  // The bot's hue carries from its face into the send button and starter chips, so each chat reads as that bot's.
  const tint = botTint(faceOf(bot.bot_id).hue, theme === 'dark')
  // No background here: the screen paints it, and a painted view would come first natively and hide the list from scrollEdgeEffects.
  return <View className="flex-1">
    <Stack.Screen options={{
      title: bot.name,
      // iOS 26 measures the edge blur on the un-inverted list and covers every message once the chat outgrows the screen.
      scrollEdgeEffects: { top: 'hidden', bottom: 'hidden', left: 'hidden', right: 'hidden' },
      // With the edge effects off, the bar keeps its own blur so messages scrolled under it stay behind frosted glass.
      headerTransparent: ios,
      // Follows the app's own appearance setting, which the system material does not see.
      headerBlurEffect: theme === 'dark' ? 'systemChromeMaterialDark' : 'systemChromeMaterialLight',
      headerTitle: () => <ScreenFaces><Link href={`/b/${bot.bot_id}/about`} asChild>
        <Pressable accessibilityRole="link" accessibilityLabel={[bot.name, workspace.primary === bot.conversation_id ? '主 Bot' : '', 'Bot 信息'].filter(Boolean).join('，')} className="-mx-2 min-h-11 shrink flex-row items-center gap-2 self-start rounded-lg px-2 active:bg-muted" style={{ maxWidth: titleWidth }}>
          <BotFace botId={bot.bot_id} mood={mood} size={28} />
          <Text numberOfLines={1} className="shrink font-semibold">{bot.name}</Text>
          {workspace.primary === bot.conversation_id ? <Icon as={Star} className="size-3.5 fill-muted-foreground text-muted-foreground" /> : null}
        </Pressable>
      </Link></ScreenFaces>,
    }} />
    <ScreenFaces><View className="flex-1">
    {/* Always mounted, so the first message sent with the keyboard open lands in the keyboard-aware list. */}
    <FlatList
      ref={list}
      data={rows}
      inverted
      keyExtractor={(row) => row.key}
      keyboardDismissMode="interactive"
      keyboardShouldPersistTaps="handled"
      renderScrollComponent={(props) => <KeyboardChatScrollView {...props} inverted offset={insets.bottom} keyboardLiftBehavior="whenAtEnd" />}
      contentContainerClassName="w-full max-w-3xl self-center px-4 py-6"
      // The inverted list's footer is its top: the oldest message starts below the transparent bar.
      ListFooterComponent={<View style={{ height: header }} />}
      // Inverted, so the leading item is the lower one: a card caused by the message above it stays close to it.
      ItemSeparatorComponent={({ leadingItem }: { leadingItem: Entry }) => <View className={'grouped' in leadingItem && leadingItem.grouped ? 'h-2' : 'attached' in leadingItem && leadingItem.attached ? 'h-3' : 'h-6'} />}
      renderItem={({ item }) => 'message' in item ? <Message message={item.message} botId={bot.bot_id} last={item.last} offline={offline} /> : <>{item.node}</>}
    />
    {rows.length ? null : <Animated.View pointerEvents="box-none" style={[StyleSheet.absoluteFill, { paddingTop: header }, above]}>
      <View pointerEvents="box-none" className="flex-1 items-center justify-center gap-4 px-4">
        <BotFace botId={bot.bot_id} mood={mood} size={96} halo={tint.glow} />
        <View className="items-center gap-1">
          <Text role="heading" className="text-center text-heading font-semibold">{bot.name}</Text>
          {workspace.primary === bot.conversation_id ? <View className="flex-row items-center gap-1">
            <Icon as={Star} className="size-3.5 fill-muted-foreground text-muted-foreground" /><Text className="text-meta text-muted-foreground">主 Bot</Text>
          </View> : null}
        </View>
        {reach === 'ready' ? <View pointerEvents="box-none" className="flex-row flex-wrap justify-center gap-2">
          {starters[bot.project ? 'project' : 'general'].map((example) => <Button key={example} variant="outline" className="rounded-full" style={{ borderColor: tint.edge }} onPress={() => setDraft({ text: example })}><Text className="text-muted-foreground">{example}</Text></Button>)}
        </View> : starting ? <View role="status" className="flex-row items-center gap-1.5"><Icon as={LoaderCircle} className="text-muted-foreground" /><Text className="text-muted-foreground">Bot 正在启动</Text></View> : <>
          <View className="flex-row items-center gap-1.5"><Icon as={MessageSquareOff} className="text-muted-foreground" /><Text className="text-muted-foreground">Bot 还不能回复</Text></View>
          {reach === 'offline' ? null : canSetUp ? <Link href="/ai" asChild>
            <Button><Icon as={Bot} /><Text>设置运行 Bot 的设备</Text></Button>
          </Link> : <View className="flex-row items-center gap-1.5"><Icon as={Crown} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">请在管理设备上设置</Text></View>}
        </>}
      </View>
    </Animated.View>}
    <RecoveryAlert top={header} />
    </View></ScreenFaces>
    <KeyboardStickyView offset={{ closed: 0, opened: insets.bottom }}>
      <View style={{ paddingBottom: insets.bottom }}>
        <Composer value={draft.text} onChange={(text) => setDraft({ text })} files={draft.files} onFiles={(files) => setDraft({ files })}
          blocked={blocked} sending={launching || (chat?.sending ?? false)} onSend={send} onStop={chat?.stoppable ? () => void workspace.actions.stop(id) : undefined} tint={tint} />
      </View>
    </KeyboardStickyView>
  </View>
}
