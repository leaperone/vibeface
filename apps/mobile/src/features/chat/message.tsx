import { Bot, CirclePause, CircleX, Clock, RotateCcw } from 'lucide-react-native'
import { memo } from 'react'
import { View } from 'react-native'
import Animated, { FadeInDown, ReduceMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { failureText, rawError, replyFailure } from '@vibeface/client/lib/errors.ts'
import { ActionButton } from '@/components/action'
import { BotFace, type FaceBot } from '@/components/bot-face'
import { Details } from '@/components/kit'
import { MessageBubble } from '@/components/arc/chat'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import type { ChatMessage } from '@/data/types'
import { easeOut, settle } from '@/lib/motion'
import { Markdown } from './markdown'
import { MessageAttachments, type ReadAttachment } from './attachments'
import { MessageActions, type MessageActionsProps } from './message-actions'

const Rise = withUniwind(Animated.View)
// As on the web: a new message rises 5px into place once.
const rise = FadeInDown.duration(settle).easing(easeOut).withInitialValues({ transform: [{ translateY: 5 }] }).reduceMotion(ReduceMotion.System)

const ended: Record<string, string> = { stopped: '已中断', interrupted: '已中断', failed: '失败' }

/**
 * The person's message in a bubble on the right; a bot's reply as plain text across the full width; another bot's in a
 * bubble under its name, as on the web. While a reply streams the bot's face moves under it, greyed while offline.
 * `retry`: 重试 after this reply failed or was stopped. Memoised: history messages keep their identity while a reply
 * streams, so only the streaming one re-renders.
 */
export const Message = memo(function Message({ message, bot, time: shown, offline, retry, attachmentScope, readAttachment, onCopy, onShare, onQuote }: { message: ChatMessage; bot: FaceBot; time?: string; offline: boolean; retry?: () => Promise<unknown>; attachmentScope?: string; readAttachment?: ReadAttachment } & MessageActionsProps) {
  const user = message.role === 'user'
  const stopped = message.state === 'interrupted' ? '已中断' : ended[message.status ?? '']
  const failed = message.status === 'failed'
  const cause = failed ? replyFailure(message.failure, message.detail) : failureText(message.failure)
  const raw = failed && message.detail ? rawError(message.detail) : ''
  const entering = message.created_at_ms > Date.now() - 2000 ? rise : undefined
  const time = shown ? <Text className="text-sm text-muted" style={{ fontVariant: ['tabular-nums'] }}>{shown}</Text> : null
  const attachments = message.attachments?.length ? <MessageAttachments files={message.attachments} scope={attachmentScope} read={readAttachment} /> : null
  const actions = <MessageActions message={message} onCopy={onCopy} onShare={onShare} onQuote={onQuote} />
  if (user) return <Rise entering={entering} className="max-w-[85%] items-end gap-1 self-end">
    <MessageBubble>
      {message.text ? <Text selectable className="text-foreground">{message.text}</Text> : null}
      {attachments}
    </MessageBubble>
    {actions}
    {message.state === 'sending' ? <Icon as={Clock} accessibilityLabel="发送中" className="text-muted" /> : time}
  </Rise>
  if (message.role === 'bot') return <Rise entering={entering} className="max-w-[85%] items-start gap-1">
    <View className="flex-row items-center gap-1.5">
      <Icon as={Bot} className="text-muted" />
      <Text numberOfLines={1} className="shrink text-sm text-muted">{message.sender?.name ?? '另一个 Bot'}</Text>
    </View>
    <MessageBubble>
      <Markdown text={message.text} />
      {attachments}
    </MessageBubble>
    {actions}
    {time}
  </Rise>
  return <Rise entering={entering} className="gap-1">
    {message.text ? <Markdown text={message.text} /> : null}
    {attachments}
    {message.state !== 'streaming' ? actions : null}
    {message.state === 'streaming' ? <View accessible accessibilityRole="image" accessibilityLabel="正在回复" className="h-6 justify-center">
      <BotFace bot={bot} mood={offline ? 'asleep' : 'replying'} size={24} />
    </View> : null}
    {stopped ? <View className="flex-row flex-wrap items-center gap-x-2 gap-y-1">
      <View className="flex-row items-center gap-1"><Icon as={failed ? CircleX : CirclePause} className={failed ? 'text-danger' : 'text-muted'} /><Text className={failed ? 'text-sm text-danger' : 'text-sm text-muted'}>{stopped}</Text></View>
      {cause ? <Text className="shrink text-sm text-muted">{cause}</Text> : null}
      {retry ? <ActionButton variant="ghost" icon={RotateCcw} action={retry}>重试</ActionButton> : null}
    </View> : null}
    {raw ? <Details><Text selectable className="font-mono text-sm text-muted">{raw}</Text></Details> : null}
    {time ? <View className="-mt-1.5">{time}</View> : null}
  </Rise>
})
