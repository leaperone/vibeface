import { Bot, CalendarClock, CirclePause, LoaderCircle } from 'lucide-react'
import { Markdown, PlainText } from '@/components/app/markdown'
import { messageKey, replyStatusLabel } from '@vibeface/client/chat/protocol.ts'
import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import { Artifact } from './artifact'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'

export type ChatMessage = TranscriptMessage & { state?: 'streaming' | 'interrupted' | 'sending' }

/** Today shows the time only; older messages also show the day. */
function shortTime(when: Date) {
  const today = when.toDateString() === new Date().toDateString()
  return when.toLocaleString('zh-CN', today ? { hour: '2-digit', minute: '2-digit' } : { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

/**
 * One message: the person's in a bubble, the bot's as plain text across the full width, another bot's in a
 * card under its name. `last` marks the end of a run from the same sender, where the time shows. `schedule` names
 * the schedule whose run produced this reply.
 */
export function Message({ message, schedule, last = true }: { message: ChatMessage; schedule?: string; last?: boolean }) {
  const { connectionOnline } = useWorkspace()
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  const full = when?.toLocaleString('zh-CN')
  const ended = message.state === 'interrupted' ? '已中断' : replyStatusLabel(message.status)
  const time = when ? <time dateTime={when.toISOString()} className={last ? `${user ? 'px-1 ' : ''}text-meta text-muted-foreground` : 'sr-only'}>{last ? shortTime(when) : full}</time> : null
  if (user) return <div className="flex min-w-0 flex-col items-end gap-1" data-message-id={messageKey(message)}>
    <div title={full} className="min-w-0 max-w-[85%] rounded-2xl bg-primary px-4 py-2 text-primary-foreground [overflow-wrap:anywhere] sm:max-w-[75%] data-[sending]:opacity-70" data-sending={message.state === 'sending' || undefined}><span className="sr-only">你：</span><PlainText text={message.text} /></div>
    {message.blobs?.map((blob) => <div key={blob.hash} className="min-w-0 max-w-[85%] sm:max-w-[75%]"><Artifact hash={blob.hash} size={blob.size} attachment /></div>)}
    {time}
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
    {message.state === 'streaming' ? <LoaderCircle aria-label="正在回复" className={`size-4 text-muted-foreground ${connectionOnline ? 'animate-spin' : ''}`} /> : null}
    {ended ? <span className="flex items-center gap-1 text-meta text-muted-foreground"><CirclePause aria-hidden="true" className="size-4" />{ended}</span> : null}
    {message.blobs?.map((blob) => <Artifact key={blob.hash} hash={blob.hash} size={blob.size} />)}
    {time}
  </div>
}
