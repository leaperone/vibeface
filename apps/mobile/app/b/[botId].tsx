import { Stack, useLocalSearchParams } from 'expo-router'
import { Bot, LoaderCircle } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { FlatList, View } from 'react-native'
import { KeyboardChatScrollView, KeyboardStickyView } from 'react-native-keyboard-controller'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Attachment, ChatMessage, Conversation } from '@/data/types'
import { Composer } from '@/features/chat/composer'
import { Message } from '@/features/chat/message'
import { QuestionCard } from '@/features/chat/question-card'

const examples = ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？']

/** One row of the chat: a message, or a question or status line between them. */
type Entry = { key: string; message: ChatMessage; last: boolean } | { key: string; node: ReactNode }

/** Questions sit right before the first message written after them, as on the web; the newest entry is first for the inverted list. */
function entries(chat: Conversation): Entry[] {
  const rows: Entry[] = []
  const questions = [...chat.questions].sort((a, b) => a.created_at_ms - b.created_at_ms)
  const sender = (message: ChatMessage | undefined) => message && (message.role === 'user' ? 'user' : 'bot')
  chat.messages.forEach((message, index) => {
    while (questions.length && questions[0].created_at_ms < message.created_at_ms) {
      const question = questions.shift()!
      rows.push({ key: `q:${question.question_id}`, node: <QuestionCard question={question} /> })
    }
    rows.push({ key: message.message_id, message, last: sender(chat.messages[index + 1]) !== sender(message) })
  })
  for (const question of questions) rows.push({ key: `q:${question.question_id}`, node: <QuestionCard question={question} /> })
  if (chat.replying && !chat.messages.some((message) => message.state === 'streaming')) rows.push({
    key: 'activity',
    node: <View accessibilityRole="progressbar" className="min-h-7 flex-row items-center gap-2"><Icon as={LoaderCircle} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{chat.status ?? '思考中'}</Text></View>,
  })
  return rows.reverse()
}

export default function ChatScreen() {
  const { botId } = useLocalSearchParams<{ botId: string }>()
  const workspace = useWorkspace()
  const insets = useSafeAreaInsets()
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const id = bot?.conversation_id
  const chat = id ? workspace.conversations[id] : undefined
  const [drafts, setDrafts] = useState<Record<string, { text: string; files: Attachment[] }>>({})
  const draft = (id && drafts[id]) || { text: '', files: [] }
  const setDraft = (next: Partial<typeof draft>) => { if (id) setDrafts((current) => ({ ...current, [id]: { ...draft, ...next } })) }
  useEffect(() => id ? workspace.actions.open(id) : undefined, [id])

  if (!bot || !id) return <EmptyState icon={Bot} title="没有这个 bot" />
  const blocked = workspace.connection.state === 'offline' ? '离线，恢复后可发送' : chat?.blocked ?? (chat ? null : '稍等')
  const send = () => {
    const { text, files } = draft
    setDraft({ text: '', files: [] })
    workspace.actions.send(id, text.trim(), files).catch(() => setDraft({ text, files }))
  }
  const rows = chat ? entries(chat) : []
  return <View className="flex-1 bg-background">
    <Stack.Screen options={{ title: bot.name }} />
    {rows.length ? <FlatList
      data={rows}
      inverted
      keyExtractor={(row) => row.key}
      keyboardDismissMode="interactive"
      keyboardShouldPersistTaps="handled"
      renderScrollComponent={(props) => <KeyboardChatScrollView {...props} inverted offset={insets.bottom} keyboardLiftBehavior="whenAtEnd" />}
      contentContainerClassName="px-4 py-6"
      ItemSeparatorComponent={() => <View className="h-6" />}
      renderItem={({ item }) => 'message' in item ? <Message message={item.message} last={item.last} /> : <>{item.node}</>}
    /> : <View className="flex-1 items-center justify-center gap-4 px-4">
      <Icon as={Bot} className="size-8 text-muted-foreground" />
      <Text role="heading" className="text-heading font-semibold">{bot.name}</Text>
      <View className="flex-row flex-wrap justify-center gap-2">
        {examples.map((example) => <Button key={example} variant="outline" className="rounded-full" onPress={() => setDraft({ text: example })}><Text>{example}</Text></Button>)}
      </View>
    </View>}
    <KeyboardStickyView offset={{ closed: 0, opened: insets.bottom }}>
      <View style={{ paddingBottom: insets.bottom }}>
        <Composer value={draft.text} onChange={(text) => setDraft({ text })} files={draft.files} onFiles={(files) => setDraft({ files })}
          blocked={blocked} onSend={send} onStop={chat?.stoppable ? () => void workspace.actions.stop(id) : undefined} />
      </View>
    </KeyboardStickyView>
  </View>
}
