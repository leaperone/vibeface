import { Link } from '@tanstack/react-router'
import { Bot, CalendarClock, ChevronDown, ChevronUp, CirclePause, CircleX, Clock, Merge, MonitorSmartphone, RotateCcw } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useState } from 'react'
import { Markdown, PlainText } from '@/components/app/markdown'
import { messageKey, replyStatusLabel } from '@vibeface/client/chat/protocol.ts'
import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import { Artifact } from './artifact'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { ActionButton } from '@/features/workspace/present'
import type { SteerOutcome } from '@vibeface/client/workspace/types.ts'

export type ChatMessage = TranscriptMessage & { state?: 'streaming' | 'interrupted' | 'sending' | 'queued' | 'merging' | 'merged'; merged?: SteerOutcome }

const mergedLabels: Record<SteerOutcome, string> = { answered: '已并入回复', unfinished: '已并入，回复未完成', unknown: '不确定是否已并入' }

/** Today shows the time only; older messages also show the day. */
function shortTime(when: Date) {
  const today = when.toDateString() === new Date().toDateString()
  return when.toLocaleString('zh-CN', today ? { hour: '2-digit', minute: '2-digit' } : { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
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
      <button type="button" aria-expanded={open} aria-label={open ? '收起' : '展开'} title={open ? '收起' : '展开'} onClick={() => setOpen(!open)}
        className="flex size-11 items-center justify-center rounded-full hover:bg-current/15 focus-visible:outline-2 focus-visible:outline-current sm:size-8">
        <Icon aria-hidden="true" className="size-4" />
      </button>
    </div>
  </>
}

/**
 * One message: the person's in a bubble, the bot's as plain text across the full width, another bot's in a
 * card under its name. `last` marks the end of a run from the same sender, where the time shows. `schedule` names
 * the schedule whose run produced this reply. `retry` sends the person's message again after this reply failed.
 */
export function Message({ message, schedule, last = true, retry }: { message: ChatMessage; schedule?: string; last?: boolean; retry?: () => Promise<void> }) {
  const { connectionOnline, data } = useWorkspace()
  const owner = data?.conversation?.owner
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  const full = when?.toLocaleString('zh-CN')
  const ended = message.state === 'interrupted' ? '已中断' : replyStatusLabel(message.status)
  const time = when ? <time dateTime={when.toISOString()} className={last ? `${user ? 'px-1 ' : ''}text-meta text-muted-foreground` : 'sr-only'}>{last ? shortTime(when) : full}</time> : null
  if (user) return <div className="flex min-w-0 flex-col items-end gap-1" data-message-id={messageKey(message)}>
    <div title={full} className="min-w-0 max-w-[85%] rounded-2xl rounded-br-md bg-secondary px-4 py-2 text-secondary-foreground ring-1 ring-border/60 [overflow-wrap:anywhere] sm:max-w-[75%] data-[sending]:opacity-70" data-sending={message.state === 'sending' || message.state === 'queued' || message.state === 'merging' || undefined}><span className="sr-only">你：</span><FoldedText text={message.text} /></div>
    {message.attachments?.map((file) => <div key={file.hash} className="w-full min-w-0 max-w-[85%] sm:max-w-[75%]"><Artifact file={file} /></div>)}
    {message.state === 'queued' ? <span className="flex items-center gap-1 px-1 text-meta text-muted-foreground"><Clock aria-hidden="true" className="size-3.5" />排队中</span>
      : message.state === 'merging' ? <span className="flex items-center gap-1 px-1 text-meta text-muted-foreground"><Merge aria-hidden="true" className="size-3.5" />正在并入当前回复</span>
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
    <div title={full} className="w-full min-w-0">
      <span className="sr-only">Bot：</span>
      <Markdown text={message.text} />
    </div>
    {message.state === 'streaming' ? <span role="img" aria-label="正在回复" className="typing-dots" data-paused={connectionOnline ? undefined : ''}><span /><span /><span /></span> : null}
    {ended === '失败' ? <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-meta">
      <span className="flex items-center gap-1 text-danger"><CircleX aria-hidden="true" className="size-4" />{ended}</span>
      {retry ? <ActionButton variant="ghost" className="-ms-1 h-8 px-2 text-meta max-md:h-11" action={retry}><RotateCcw aria-hidden="true" />再发一次</ActionButton> : null}
      {/* The failure text points at the device's AI settings; the device running this chat holds them. */}
      {owner ? <Button asChild variant="ghost" className="-ms-1 h-8 px-2 text-meta max-md:h-11"><Link to="/devices/$deviceId" params={{ deviceId: owner }}><MonitorSmartphone aria-hidden="true" />检查设备</Link></Button> : null}
    </span>
      : ended ? <span className="flex items-center gap-1 text-meta text-muted-foreground"><CirclePause aria-hidden="true" className="size-4" />{ended}</span> : null}
    {time}
  </div>
}
