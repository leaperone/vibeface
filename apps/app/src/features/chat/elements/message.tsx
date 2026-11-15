import { BotFace } from '@/components/app/bot-face'
import { Markdown,PlainText } from '@/components/app/markdown'
import styles from "@/components/arc/conversation.module.css"
import { Details } from '@/components/arc/product-kit'
import { RoutedAction } from "@/components/arc/routed-action"
import { ActionButton } from '@/features/workspace/present'
import { messageKey,replyStatusLabel } from '@vibeface/client/chat/protocol.ts'
import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import { botModel,failureText,rawError,replyFailure } from '@vibeface/client/lib/errors.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import type { SteerOutcome } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Bot,CalendarClock,ChevronDown,ChevronUp,CirclePause,CircleQuestionMark,CircleX,Clock,LoaderCircle,Merge,MonitorSmartphone,RotateCcw } from 'lucide-react'
import { useState } from 'react'
import { Artifact } from './artifact'

export type ChatMessage = TranscriptMessage & { state?: 'streaming' | 'interrupted' | 'sending' | 'queued' | 'merging' | 'merged'; merged?: SteerOutcome }

const mergedLabels: Record<SteerOutcome, string> = { answered: '关联回合已完成', unfinished: '关联回合未完成', unknown: '补充结果未知' }

/** A long message shows its first lines until expanded. */
function isLong(text: string) {
  return text.length > 600 || text.split('\n').length > 12
}

export function FoldedText({ text }: { text: string }) {
  const [open, setOpen] = useState(false)
  if (!isLong(text)) return <PlainText text={text} />
  const Icon = open ? ChevronUp : ChevronDown
  return <>
    <div className={open ? undefined : 'line-clamp-8'}><PlainText text={text} /></div>
    <div className="-mr-2 -mb-1 flex justify-end">
      <Button variant="ghost" type="button" aria-expanded={open} aria-label={open ? '收起' : '展开'} title={open ? '收起' : '展开'} onClick={() => setOpen(!open)}
        className="flex items-center justify-center">
        <Icon aria-hidden="true" className="size-4" />
      </Button>
    </div>
  </>
}

/**
 * One message: the person's in a bubble, the bot's as plain text across the full width, another bot's in a
 * card under its name. `time` shows under the last message of each minute. `schedule` names
 * the schedule whose run produced this reply. `retry` is 重试 after this reply failed or was stopped.
 */
/** `detail`: the device's own words for a failed reply, folded under its cause. */
export function Message({ message, conversationId, schedule, time: shown, retry, detail }: { message: ChatMessage; conversationId?: string; schedule?: string; time?: string; retry?: () => Promise<void>; detail?: string }) {
  const { connectionOnline, data, selectedBot, refresh } = useWorkspace()
  const owner = data?.conversation?.owner
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  const full = when?.toLocaleString('zh-CN')
  const ended = message.state === 'interrupted' ? '已中断' : replyStatusLabel(message.status)
  // Why it failed, in the reader's words; the original text folds under it on the device that ran it.
  const failed = ended === '失败'
  const cause = failed ? replyFailure(message.failure, detail) : failureText(message.failure)
  const raw = failed && detail ? rawError(detail) : ''
  const again = retry ? <ActionButton variant="ghost" className="-ms-1" action={retry}><RotateCcw aria-hidden="true" />重试</ActionButton> : null
  const time = when ? <time dateTime={when.toISOString()} className={shown ? `${user ? 'px-1 ' : ''}text-sm text-secondary` : 'sr-only'}>{shown ?? full}</time> : null
  if (user) return <div className="flex min-w-0 flex-col items-end gap-1" data-message-id={messageKey(message)}>
    {message.text ? <div title={full} className={styles.user} data-sending={message.state === 'sending' || message.state === 'queued' || message.state === 'merging' || undefined}><span className="sr-only">你：</span><FoldedText text={message.text} /></div> : null}
    {message.attachments?.map((file) => <div key={`${conversationId ?? ''}:${file.hash}`} className="w-full min-w-0 max-w-[85%] sm:max-w-[75%]"><Artifact file={file} conversationId={conversationId} /></div>)}
    {message.state === 'sending' ? <span role="status" className="px-1 text-sm text-secondary">正在确认送达</span>
      : message.state === 'queued' ? <span role="status" className="flex items-center gap-1 px-1 text-sm text-secondary"><Clock aria-hidden="true" className="size-3.5" />已排队</span>
      : message.state === 'merging' ? <span role="status" className="flex items-center gap-1 px-1 text-sm text-secondary"><Merge aria-hidden="true" className="size-3.5" />已登记为回合补充</span>
      : message.state === 'merged' && (!message.merged || message.merged === 'unknown') ? <div className="flex max-w-[85%] flex-wrap items-center justify-end gap-x-2 gap-y-1 text-sm sm:max-w-[75%]">
        <span role="status" className="flex items-center gap-1 text-warning"><CircleQuestionMark aria-hidden="true" className="size-3.5 shrink-0" />{mergedLabels.unknown}</span>
        <ActionButton variant="ghost" className="" disabled={!connectionOnline} action={refresh}>再查一次</ActionButton>
      </div>
      : message.state === 'merged' ? <span role="status" className="flex items-center gap-1 px-1 text-sm text-secondary"><Merge aria-hidden="true" className="size-3.5" />{mergedLabels[message.merged ?? 'unknown']}</span> : time}
  </div>
  if (message.role === 'bot') return <div className="flex min-w-0 flex-col items-start gap-1" data-message-id={messageKey(message)}>
    <span className="flex min-w-0 max-w-full items-center gap-1.5 text-sm text-secondary">
      <Bot aria-hidden="true" className="size-4 shrink-0" />
      <span className="min-w-0 truncate">{message.sender?.name ?? '另一个 Bot'}</span>
    </span>
    <div title={full} className={styles.bot}>
      <span className="sr-only">{message.sender?.name ?? '另一个 Bot'}：</span>
      <Markdown text={message.text} streaming={message.state === 'streaming'} />
    </div>
    {time}
  </div>
  return <div className="flex min-w-0 flex-col items-start gap-1" data-message-id={messageKey(message)}>
    {schedule ? <span className="flex min-w-0 max-w-full items-center gap-1.5 text-sm text-secondary">
      <CalendarClock aria-hidden="true" className="size-4 shrink-0" />
      {schedule === '定时任务' ? null : <span className="sr-only">定时任务：</span>}
      <span className="min-w-0 truncate">{schedule}</span>
    </span> : null}
    {/* Every streamed chunk redraws the reply inside the chat log, which would read it from the top each time; the finished reply lands as a new entry and is read once. */}
    <div title={full} aria-live={message.state === 'streaming' ? 'off' : undefined} className={styles.assistant}>
      <span className="sr-only">Bot：</span>
      <Markdown text={message.text} streaming={message.state === 'streaming'} />
    </div>
    {message.state === 'streaming' ? <span role="img" aria-label="正在回复" className="flex h-6 items-center">{selectedBot ? <BotFace bot={selectedBot} mood={connectionOnline ? 'replying' : 'asleep'} size={24} /> : <LoaderCircle aria-hidden="true" className={`size-4 text-secondary ${connectionOnline ? 'animate-spin' : ''}`} />}</span> : null}
    {failed ? <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
      <span className="flex items-center gap-1 text-danger"><CircleX aria-hidden="true" className="size-4" />{ended}</span>
      {cause ? <span className="text-secondary">{cause}</span> : null}
      {again}
      {/* The failure text points at the device's AI settings, which the device running this chat holds, unless the bot's own model caused it. */}
      {owner && !botModel(detail) ? <RoutedAction variant="ghost" className="-ms-1" to="/devices/$deviceId" params={{ deviceId: owner }} search={{ bot: selectedBot?.bot_id }}><MonitorSmartphone aria-hidden="true" />检查设备</RoutedAction> : null}
    </span>
      : ended ? <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-secondary"><span className="flex items-center gap-1"><CirclePause aria-hidden="true" className="size-4" />{ended}</span>{cause ? <span>{cause}</span> : null}{again}</span> : null}
    {raw ? <Details className="-mt-1">{raw}</Details> : null}
    {time}
  </div>
}
