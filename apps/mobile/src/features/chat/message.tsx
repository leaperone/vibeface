import { Bot, CirclePause, CircleX, Clock } from 'lucide-react-native'
import { View } from 'react-native'
import Animated, { FadeInDown, ReduceMotion, useReducedMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { failureText, replyFailure } from '@vibeface/client/lib/errors.ts'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import type { ChatMessage } from '@/data/types'
import { easeOut } from '@/lib/motion'
import { Markdown } from './markdown'

const Rise = withUniwind(Animated.View)
// As on the web: a new message rises 5px into place once.
const rise = FadeInDown.duration(180).easing(easeOut).withInitialValues({ transform: [{ translateY: 5 }] }).reduceMotion(ReduceMotion.System)

const ended: Record<string, string> = { stopped: '已中断', interrupted: '已中断', failed: '失败' }

function shortTime(at: number) {
  const when = new Date(at)
  const pad = (value: number) => String(value).padStart(2, '0')
  const clock = `${pad(when.getHours())}:${pad(when.getMinutes())}`
  return when.toDateString() === new Date().toDateString() ? clock : `${when.getMonth() + 1}/${when.getDate()} ${clock}`
}

const breathe = {
  animationName: { '0%': { opacity: 0.3, transform: [{ translateY: 0 }] }, '30%': { opacity: 1, transform: [{ translateY: -2 }] }, '60%': { opacity: 0.3, transform: [{ translateY: 0 }] }, '100%': { opacity: 0.3, transform: [{ translateY: 0 }] } },
  animationDuration: '1.2s', animationIterationCount: 'infinite', animationTimingFunction: 'ease-in-out',
} as const

/** A reply still streaming, as the web's typing dots: they breathe one after another, and hold still and dim while offline. */
function TypingDots({ paused }: { paused: boolean }) {
  const still = useReducedMotion() || paused
  return <View accessible accessibilityRole="image" accessibilityLabel="正在回复" className="h-6 flex-row items-center gap-1">
    {[0, 150, 300].map((delay) => <Rise key={delay} className="size-1.5 rounded-full bg-muted-foreground" style={still ? { opacity: 0.5 } : { ...breathe, animationDelay: `${delay}ms` }} />)}
  </View>
}

/**
 * The person's message in a bubble on the right; a bot's reply as plain text across the full width; another bot's in a
 * bubble under its name, as on the web. `offline` pauses a streaming reply's dots.
 */
export function Message({ message, last, offline }: { message: ChatMessage; last: boolean; offline: boolean }) {
  const user = message.role === 'user'
  const stopped = message.state === 'interrupted' ? '已中断' : ended[message.status ?? '']
  const failed = message.status === 'failed'
  const cause = failed ? replyFailure(message.failure) : failureText(message.failure)
  const entering = message.created_at_ms > Date.now() - 2000 ? rise : undefined
  const time = last && message.created_at_ms ? <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{shortTime(message.created_at_ms)}</Text> : null
  if (user) return <Rise entering={entering} className="max-w-[85%] items-end gap-1 self-end">
    <View className="rounded-2xl rounded-br-md bg-secondary px-4 py-2 ring-1 ring-border/60">
      <Text selectable className="text-secondary-foreground">{message.text}</Text>
    </View>
    {message.state === 'sending' ? <Icon as={Clock} accessibilityLabel="发送中" className="text-muted-foreground" /> : time}
  </Rise>
  if (message.role === 'bot') return <Rise entering={entering} className="max-w-[85%] items-start gap-1">
    <View className="flex-row items-center gap-1.5">
      <Icon as={Bot} className="text-muted-foreground" />
      <Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{message.sender?.name ?? '另一个 Bot'}</Text>
    </View>
    <View className="rounded-2xl border border-border bg-muted px-4 py-2">
      <Markdown text={message.text} />
    </View>
    {time}
  </Rise>
  return <Rise entering={entering} className="gap-1">
    {message.text ? <Markdown text={message.text} /> : null}
    {message.state === 'streaming' ? <TypingDots paused={offline} /> : null}
    {stopped ? <View className="flex-row flex-wrap items-center gap-x-2 gap-y-1">
      <View className="flex-row items-center gap-1"><Icon as={failed ? CircleX : CirclePause} className={failed ? 'text-danger' : 'text-muted-foreground'} /><Text className={failed ? 'text-meta text-danger' : 'text-meta text-muted-foreground'}>{stopped}</Text></View>
      {cause ? <Text className="shrink text-meta text-muted-foreground">{cause}</Text> : null}
    </View> : null}
    {time ? <View className="-mt-1.5">{time}</View> : null}
  </Rise>
}
