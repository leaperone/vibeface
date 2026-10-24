import { CirclePause, Clock, LoaderCircle } from 'lucide-react-native'
import { View } from 'react-native'
import Animated, { FadeInDown, ReduceMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import type { ChatMessage } from '@/data/types'
import { Markdown } from './markdown'

const Rise = withUniwind(Animated.View)

const ended: Record<string, string> = { stopped: '已中断', interrupted: '已中断', failed: '失败' }

function shortTime(at: number) {
  const when = new Date(at)
  const pad = (value: number) => String(value).padStart(2, '0')
  const clock = `${pad(when.getHours())}:${pad(when.getMinutes())}`
  return when.toDateString() === new Date().toDateString() ? clock : `${when.getMonth() + 1}/${when.getDate()} ${clock}`
}

/** The person's message in a bubble on the right; a bot's reply as plain text across the full width, as on the web. */
export function Message({ message, last }: { message: ChatMessage; last: boolean }) {
  const user = message.role === 'user'
  const stopped = message.state === 'interrupted' ? '已中断' : ended[message.status ?? '']
  const entering = message.created_at_ms > Date.now() - 2000 ? FadeInDown.duration(220).reduceMotion(ReduceMotion.System) : undefined
  const time = last && message.created_at_ms ? <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{shortTime(message.created_at_ms)}</Text> : null
  if (user) return <Rise entering={entering} className="max-w-[85%] items-end gap-1 self-end">
    <View className="rounded-3xl bg-muted px-4 py-2.5">
      <Text selectable>{message.text}</Text>
    </View>
    {message.state === 'sending' ? <Icon as={Clock} accessibilityLabel="发送中" className="size-4 text-muted-foreground" /> : time}
  </Rise>
  return <Rise entering={entering} className="gap-1">
    {message.text ? <Markdown text={message.text} /> : null}
    {message.state === 'streaming' ? <Icon as={LoaderCircle} accessibilityLabel="正在回复" className="size-4 text-muted-foreground" /> : null}
    {stopped ? <View className="flex-row items-center gap-1"><Icon as={CirclePause} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{stopped}</Text></View> : null}
    {time ? <View className="-mt-1.5">{time}</View> : null}
  </Rise>
}
