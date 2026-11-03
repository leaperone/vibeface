import { Link } from '@tanstack/react-router'
import { Bot, CalendarClock, ChevronDown, ChevronUp, CirclePause, CircleX, Clock, LoaderCircle, Merge, MonitorSmartphone, RotateCcw } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useState } from 'react'
import { Markdown, PlainText } from '@/components/app/markdown'
import { Details } from '@/components/app/kit'
import { BotFace } from '@/components/app/bot-face'
import { messageKey, replyStatusLabel } from '@vibeface/client/chat/protocol.ts'
import { botModel, failureText, rawError, replyFailure } from '@vibeface/client/lib/errors.ts'
import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import { Artifact } from './artifact'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { ActionButton } from '@/features/workspace/present'
import type { SteerOutcome } from '@vibeface/client/workspace/types.ts'

export type ChatMessage = TranscriptMessage & { state?: 'streaming' | 'interrupted' | 'sending' | 'queued' | 'merging' | 'merged'; merged?: SteerOutcome }

const mergedLabels: Record<SteerOutcome, string> = { answered: '已补充给 Bot', unfinished: '补充后回复没完成', unknown: '不确定 Bot 看到了没有' }

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
      <button type="button" aria-expanded={open} aria-label={open ? '收起' : '展开'} title={open ? '收起' : '展开'} onClick={() => setOpen(!open)}
        className="flex size-11 items-center justify-center rounded-full hover:bg-current/15 focus-visible:outline-2 focus-visible:outline-current sm:size-8">
        <Icon aria-hidden="true" className="size-4" />
      </button>
    </div>
  </>
}

/**
 * One message: the person's in a bubble, the bot's as plain text across the full width, another bot's in a
 * card under its name. `time` shows under the last message of each minute. `schedule` names
 * the schedule whose run produced this reply. `retry` is 重试 after this reply failed or was stopped.
 */
/** `detail`: the device's own words for a failed reply, folded under its cause. */
export function Message({ message, schedule, time: shown, retry, detail }: { message: ChatMessage; schedule?: string; time?: string; retry?: () => Promise<void>; detail?: string }) {
  const { connectionOnline, data, selectedBot } = useWorkspace()
  const owner = data?.conversation?.owner
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  const full = when?.toLocaleString('zh-CN')
  const ended = message.state === 'interrupted' ? '已中断' : replyStatusLabel(message.status)
  // Why it failed, in the reader's words; the original text folds under it on the device that ran it.
  const failed = ended === '失败'
  const cause = failed ? replyFailure(message.failure, detail) : failureText(message.failure)
  const raw = failed && detail ? rawError(detail) : ''
  const again = retry ? <ActionButton variant="ghost" className="-ms-1 h-8 px-2 text-meta max-md:h-11" action={retry}><RotateCcw aria-hidden="true" />重试</ActionButton> : null
  const time = when ? <time dateTime={when.toISOString()} className={shown ? `${user ? 'px-1 ' : ''}text-meta text-muted-foreground` : 'sr-only'}>{shown ?? full}</time> : null
  if (user) return <div className="flex min-w-0 flex-col items-end gap-1" data-message-id={messageKey(message)}>
    {message.text ? <div title={full} className="min-w-0 max-w-[85%] rounded-2xl rounded-br-md bg-secondary px-4 py-2 text-secondary-foreground ring-1 ring-border/60 [overflow-wrap:anywhere] sm:max-w-[75%] data-[sending]:opacity-70" data-sending={message.state === 'sending' || message.state === 'queued' || message.state === 'merging' || undefined}><span className="sr-only">你：</span><FoldedText text={message.text} /></div> : null}
    {message.attachments?.map((file) => <div key={file.hash} className="w-full min-w-0 max-w-[85%] sm:max-w-[75%]"><Artifact file={file} /></div>)}
    {message.state === 'queued' ? <span className="flex items-center gap-1 px-1 text-meta text-muted-foreground"><Clock aria-hidden="true" className="size-3.5" />排队中</span>
      : message.state === 'merging' ? <span className="flex items-center gap-1 px-1 text-meta text-muted-foreground"><Merge aria-hidden="true" className="size-3.5" />Bot 正在看这条补充</span>
      : message.state === 'merged' ? <span className="flex items-center gap-1 px-1 text-meta text-muted-foreground"><Merge aria-hidden="true" className="size-3.5" />{mergedLabels[message.merged ?? 'answered']}</span> : time}
  </div>
  if (message.role === 'bot') return <div className="flex min-w-0 flex-col items-start gap-1" data-message-id={messageKey(message)}>
    <span className="flex min-w-0 max-w-full items-center gap-1.5 text-meta text-muted-foreground">
      <Bot aria-hidden="true" className="size-4 shrink-0" />
      <span className="min-w-0 truncate">{message.sender?.name ?? '另一个 Bot'}</span>
    </span>
    <div title={full} className="min-w-0 max-w-[85%] rounded-2xl border bg-muted px-4 py-2 [overflow-wrap:anywhere] sm:max-w-[75%]">
      <span className="sr-only">{message.sender?.name ?? '另一个 Bot'}：</span>
      <Markdown text={message.text} />
    </div>
    {time}
  </div>
  return <div className="flex min-w-0 flex-col items-start gap-1" data-message-id={messageKey(message)}>
    {schedule ? <span className="flex min-w-0 max-w-full items-center gap-1.5 text-meta text-muted-foreground">
      <CalendarClock aria-hidden="true" className="size-4 shrink-0" />
      {schedule === '定时任务' ? null : <span className="sr-only">定时任务：</span>}
      <span className="min-w-0 truncate">{schedule}</span>
    </span> : null}
    {/* Every streamed chunk redraws the reply inside the chat log, which would read it from the top each time; the finished reply lands as a new entry and is read once. */}
    <div title={full} aria-live={message.state === 'streaming' ? 'off' : undefined} className="w-full min-w-0">
      <span className="sr-only">Bot：</span>
      <Markdown text={message.text} />
    </div>
    {message.state === 'streaming' ? <span role="img" aria-label="正在回复" className="flex h-6 items-center">{selectedBot ? <BotFace bot={selectedBot} mood={connectionOnline ? 'replying' : 'asleep'} size={24} /> : <LoaderCircle aria-hidden="true" className={`size-4 text-muted-foreground ${connectionOnline ? 'animate-spin' : ''}`} />}</span> : null}
    {failed ? <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-meta">
      <span className="flex items-center gap-1 text-danger"><CircleX aria-hidden="true" className="size-4" />{ended}</span>
      {cause ? <span className="text-muted-foreground">{cause}</span> : null}
      {again}
      {/* The failure text points at the device's AI settings, which the device running this chat holds, unless the bot's own model caused it. */}
      {owner && !botModel(detail) ? <Button asChild variant="ghost" className="-ms-1 h-8 px-2 text-meta max-md:h-11"><Link to="/devices/$deviceId" params={{ deviceId: owner }} search={{ bot: selectedBot?.bot_id }}><MonitorSmartphone aria-hidden="true" />检查设备</Link></Button> : null}
    </span>
      : ended ? <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-meta text-muted-foreground"><span className="flex items-center gap-1"><CirclePause aria-hidden="true" className="size-4" />{ended}</span>{cause ? <span>{cause}</span> : null}{again}</span> : null}
    {raw ? <Details className="-mt-1">{raw}</Details> : null}
    {time}
  </div>
}
