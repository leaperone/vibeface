import { BotFace } from '@/components/app/bot-face'
import { Markdown,PlainText } from '@/components/app/markdown'
import styles from "@/components/arc/conversation.module.css"
import { ActionButton } from '@/features/workspace/present'
import { deliveryMark as markOf,messageKey,replyStatusLabel } from '@vibeface/client/chat/protocol.ts'
import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import { failureText,replyFailure } from '@vibeface/client/lib/errors.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import type { SteerOutcome } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Bot,CalendarClock,ChevronDown,ChevronUp,CirclePause,CircleQuestionMark,CircleX,Clock,LoaderCircle,Merge,RotateCcw } from 'lucide-react'
import { useState } from 'react'
import { Artifact } from './artifact'
import { MemoryPreference } from './memory-preference'

export type ChatMessage = TranscriptMessage & { state?: 'streaming' | 'interrupted' | 'sending' | 'queued' | 'merging' | 'merged'; merged?: SteerOutcome }

function deliveryMark(message: ChatMessage) {
  const mark = markOf(message.state, message.merged)
  if (!mark) return null
  const { label } = mark
  const unknown = mark.kind === 'unknown'
  const Icon = unknown ? CircleQuestionMark : mark.kind === 'merge' ? Merge : Clock
  return <span className={`flex items-center gap-1 text-sm ${unknown ? 'text-warning' : 'text-secondary'}`}><span role="img" aria-label={label} title={label}><Icon aria-hidden="true" className="size-3.5" /></span>{unknown ? <span aria-hidden="true">未确认</span> : null}</span>
}

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
/** `detail`: the device's own words for a failed reply, read for its cause. */
export function Message({ message, conversationId, schedule, time: shown, retry, detail, remember = false }: { message: ChatMessage; conversationId?: string; schedule?: string; time?: string; retry?: () => Promise<void>; detail?: string; remember?: boolean }) {
  const { connectionOnline, selectedBot } = useWorkspace()
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  const full = when?.toLocaleString('zh-CN')
  const ended = message.state === 'interrupted' ? '已中断' : replyStatusLabel(message.status)
  const failed = ended === '失败'
  const cause = failed ? replyFailure(message.failure, detail) : failureText(message.failure)
  const again = retry ? <ActionButton variant="ghost" className="-ms-1" action={retry}><RotateCcw aria-hidden="true" />重试</ActionButton> : null
  const time = when ? <time dateTime={when.toISOString()} className={shown ? 'text-sm text-secondary' : 'sr-only'}>{shown ?? full}</time> : null
  const mark = user ? deliveryMark(message) : null
  if (user) return <div className="flex min-w-0 flex-col items-end gap-1" data-message-id={messageKey(message)}>
    {message.text ? <div title={full} className={styles.user} data-sending={message.state === 'sending' || message.state === 'queued' || message.state === 'merging' || undefined}><span className="sr-only">你：</span><FoldedText text={message.text} /></div> : null}
    {message.attachments?.map((file) => <div key={`${conversationId ?? ''}:${file.hash}`} className="w-full min-w-0 max-w-[85%] sm:max-w-[75%]"><Artifact file={file} conversationId={conversationId} /></div>)}
    {/* Always mounted so a delivery state that appears later is announced. */}
    <span aria-live="polite" className="flex items-center gap-1 px-1">{mark}{time}</span>
    {remember && conversationId ? <MemoryPreference key={messageKey(message)} message={message} conversationId={conversationId} /> : null}
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
    </span>
      : ended ? <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-secondary"><span className="flex items-center gap-1"><CirclePause aria-hidden="true" className="size-4" />{ended}</span>{cause ? <span>{cause}</span> : null}{again}</span> : null}
    {time}
  </div>
}
