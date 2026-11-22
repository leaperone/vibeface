import { Stack, useFocusEffect, useLocalSearchParams } from 'expo-router'
import { ListChecks } from 'lucide-react-native'
import { useCallback, useRef, useState, type ReactNode } from 'react'
import { View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Text } from '@/components/arc/text'
import { useLive } from '@/data/live'
import { useChatExtras } from '@/features/chat/work'

/** Everything about a bot's chat besides its messages: what needs the person, inputs on their way, tasks, changes and runtimes. */
export default function BotActivity() {
  const { botId } = useLocalSearchParams<{ botId: string }>()
  const shared = useLive()
  const bot = shared?.bots.find((item) => item.bot_id === botId)
  const [error, setError] = useState<{ generation: number; botId: string; text: string } | null>(null)
  const [retry, setRetry] = useState(0)
  const latest = useRef(shared)
  latest.current = shared
  useFocusEffect(useCallback(() => {
    const shared = latest.current
    if (!shared) return
    shared.setView('other')
    let alive = true
    const current = () => alive && shared.isCurrent()
    if (bot && shared.connectionOnline && (retry > 0 || !shared.synced || shared.conversationId !== bot.conversation_id || shared.data?.conversation?.conversation_id !== bot.conversation_id)) {
      setError(null)
      void shared.selectBot(bot).then(() => { if (current()) setError(null) }, (cause: unknown) => {
        if (current()) setError({ generation: shared.generation, botId, text: messageOf(cause, '读取动态') })
      })
    }
    return () => { alive = false }
  }, [bot?.conversation_id, shared?.generation, shared?.connectionOnline, retry]))
  const { activity } = useChatExtras(bot?.conversation_id ?? '', botId)
  const loaded = shared && bot && shared.conversationId === bot.conversation_id && shared.data?.conversation?.conversation_id === bot.conversation_id
  const readError = error?.generation === shared?.generation && error?.botId === botId ? error.text : null
  const title = <Stack.Screen options={{ title: '动态' }} />
  if (!loaded) return <>{title}<EmptyState icon={ListChecks}
    title={readError ?? (!shared || !shared.connectionOnline ? '连接后再读取' : !bot ? '找不到这个 Bot' : '正在读取')}
    action={shared?.connectionOnline && bot ? <Button variant="secondary" onPress={() => setRetry((value) => value + 1)}><Text>重新读取</Text></Button> : undefined} className="flex-1 justify-center" /></>
  const sections: [string, ReactNode[]][] = [['要处理', activity.todo], ['发送中', activity.sending], ['任务', activity.tasks], ['最近改动', activity.changes], ['运行信息', activity.runtime]]
  const shown = sections.filter(([, items]) => items.length)
  if (!shown.length) return <>{title}<EmptyState icon={ListChecks} title="暂无动态" className="flex-1 justify-center" /></>
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-8 p-4 pb-10">
    {title}
    {shown.map(([heading, items]) => <View key={heading} className="gap-3">
      <Text role="heading" className="px-1 text-sm font-medium text-muted">{heading}</Text>
      {items}
    </View>)}
  </KeyboardAwareScrollView>
}
