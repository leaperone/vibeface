import { CirclePause, LoaderCircle, Repeat } from 'lucide-react'
import { Markdown, PlainText } from '@/components/app/markdown'
import { messageKey, replyStatusLabel } from '../protocol.ts'
import type { TranscriptMessage } from '../types.ts'
import { Artifact } from './artifact'

export type ChatMessage = TranscriptMessage & { state?: 'streaming' | 'interrupted' | 'sending' }

/** `routine` names the routine whose run produced this reply. */
export function Message({ message, routine }: { message: ChatMessage; routine?: string }) {
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  const ended = message.state === 'interrupted' ? '已中断' : replyStatusLabel(message.status)
  const time = when ? <time className="sr-only" dateTime={when.toISOString()}>{when.toLocaleString('zh-CN')}</time> : null
  if (user) return <div className="flex justify-end" data-message-id={messageKey(message)}>
    <div className="grid min-w-0 max-w-[85%] gap-2">
      <div className="min-w-0 rounded-2xl bg-muted px-4 py-2.5 [overflow-wrap:anywhere]"><span className="sr-only">你：</span><PlainText text={message.text} />{time}</div>
      {message.blobs?.map((blob) => <Artifact key={blob.hash} hash={blob.hash} size={blob.size} attachment />)}
    </div>
  </div>
  return <div className="grid min-w-0 gap-2" data-message-id={messageKey(message)}>
    <span className="sr-only">助手：</span>
    {routine ? <span className="flex min-w-0 items-center gap-1.5 text-meta text-muted-foreground">
      <Repeat aria-hidden="true" className="size-4 shrink-0" />
      {routine === '例行任务' ? null : <span className="sr-only">例行任务：</span>}
      <span className="min-w-0 truncate">{routine}</span>
    </span> : null}
    <Markdown text={message.text} />
    {message.state === 'streaming' ? <LoaderCircle aria-label="正在回复" className="size-4 animate-spin text-muted-foreground" /> : null}
    {ended ? <span className="flex items-center gap-1 text-meta text-muted-foreground"><CirclePause aria-hidden="true" className="size-4" />{ended}</span> : null}
    {time}
    {message.blobs?.map((blob) => <Artifact key={blob.hash} hash={blob.hash} size={blob.size} />)}
  </div>
}
