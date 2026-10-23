import { Link, Stack, useLocalSearchParams } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { Bot, LoaderCircle } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { FlatList, Platform, StyleSheet, View } from 'react-native'
import { KeyboardChatScrollView, KeyboardStickyView, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Animated, { useAnimatedStyle } from 'react-native-reanimated'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { useUniwind } from 'uniwind'
import { EmptyState } from '@/components/kit'
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

const examples = ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？']

/** One row of the chat: a message, or a question or status line between them. */
type Entry = { key: string; message: ChatMessage; last: boolean } | Row

/**
 * Questions sit right before the first message written after them, tasks and changes right after the message that caused
 * them and what waits on the person last, as on the web; the newest entry is first for the inverted list.
 */
function entries(chat: Conversation, extras: ReturnType<typeof useChatExtras>): Entry[] {
  const rows: Entry[] = []
  const questions = [...chat.questions].sort((a, b) => a.created_at_ms - b.created_at_ms)
  chat.messages.forEach((message, index) => {
    while (questions.length && questions[0].created_at_ms < message.created_at_ms) {
      const question = questions.shift()!
      rows.push({ key: `q:${question.question_id}`, node: <QuestionCard question={question} /> })
    }
    const next = chat.messages[index + 1]
    // The time closes a burst: the newest message, or one followed by a quiet spell, not every turn.
    rows.push({ key: message.message_id, message, last: !next || next.created_at_ms - message.created_at_ms > 5 * 60_000 })
    rows.push(...extras.after.get(message.message_id) ?? [])
  })
  for (const question of questions) rows.push({ key: `q:${question.question_id}`, node: <QuestionCard question={question} /> })
  rows.push(...extras.rest)
  if (chat.replying && !chat.messages.some((message) => message.state === 'streaming')) rows.push({
    key: 'activity',
    node: <View accessibilityRole="progressbar" className="min-h-7 flex-row items-center gap-2"><Icon as={LoaderCircle} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{chat.status ?? '思考中'}</Text></View>,
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
  const { theme } = useUniwind()
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const id = bot?.conversation_id
  const chat = id ? workspace.conversations[id] : undefined
  const [drafts, setDrafts] = useState<Record<string, { text: string; files: Attachment[] }>>({})
  const draft = (id && drafts[id]) || { text: '', files: [] }
  const setDraft = (next: Partial<typeof draft>) => { if (id) setDrafts((current) => ({ ...current, [id]: { ...draft, ...next } })) }
  useEffect(() => id ? workspace.actions.open(id) : undefined, [id])
  const extras = useChatExtras(id ?? '', botId)
  const live = useLive()
  // No device can run a bot at all: the entry device's AI settings set one up; an offline one only needs to come back.
  const noAgent = Boolean(live && !live.nodes.some((node) => !node.revoked && node.capabilities?.manager))
  const keyboard = useReanimatedKeyboardAnimation()
  const list = useRef<FlatList<ReturnType<typeof entries>[number]>>(null)
  // The empty state stays centred in the space above the composer instead of riding up with the list.
  const above = useAnimatedStyle(() => ({ paddingBottom: Math.max(0, Math.abs(keyboard.height.value) - insets.bottom) }))

  if (!bot || !id) return <EmptyState icon={Bot} title="没有这个 bot" />
  const blocked = workspace.connection.state === 'offline' ? '离线，恢复后可发送' : chat?.blocked ?? (chat ? null : '稍等')
  const send = () => {
    const { text, files } = draft
    setDraft({ text: '', files: [] })
    list.current?.scrollToOffset({ offset: 0, animated: true })
    workspace.actions.send(id, text.trim(), files).catch(() => setDraft({ text, files }))
  }
  const rows = chat ? entries(chat, extras) : []
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
      headerRight: () => <Link href={`/b/${bot.bot_id}/about`} asChild>
        <Button variant="ghost" size="icon" accessibilityLabel="bot 信息"><Icon as={Bot} /></Button>
      </Link>,
    }} />
    <View className="flex-1">
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
      ItemSeparatorComponent={() => <View className="h-6" />}
      renderItem={({ item }) => 'message' in item ? <Message message={item.message} last={item.last} /> : <>{item.node}</>}
    />
    {rows.length ? null : <Animated.View pointerEvents="box-none" style={[StyleSheet.absoluteFill, { paddingTop: header }, above]}>
      <View pointerEvents="box-none" className="flex-1 items-center justify-center gap-4 px-4">
        <Icon as={Bot} className="size-8 text-muted-foreground" />
        <Text role="heading" className="text-heading font-semibold">{bot.name}</Text>
        <View pointerEvents="box-none" className="flex-row flex-wrap justify-center gap-2">
          {examples.map((example) => <Button key={example} variant="outline" className="rounded-full" onPress={() => setDraft({ text: example })}><Text>{example}</Text></Button>)}
        </View>
      </View>
    </Animated.View>}
    </View>
    <KeyboardStickyView offset={{ closed: 0, opened: insets.bottom }}>
      <View style={{ paddingBottom: insets.bottom }}>
        <Composer value={draft.text} onChange={(text) => setDraft({ text })} files={draft.files} onFiles={(files) => setDraft({ files })}
          blocked={blocked} onSend={send} onStop={chat?.stoppable ? () => void workspace.actions.stop(id) : undefined} />
        {noAgent ? <Link href="/ai" asChild>
          <Button variant="link" className="-mt-2 min-h-11 self-start px-6"><Text>去配置</Text></Button>
        </Link> : null}
      </View>
    </KeyboardStickyView>
  </View>
}
