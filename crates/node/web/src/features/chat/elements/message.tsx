import { Bubble, BubbleContent } from '@/components/ui/bubble'
import { Message as MessageRoot, MessageContent, MessageHeader } from '@/components/ui/message'

import { messageKey, roleLabel } from '../protocol.ts'
import type { TranscriptMessage } from '../types.ts'
import { Artifact } from './artifact'

export function Message({ message }: { message: TranscriptMessage }) {
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  return (
    <MessageRoot align={user ? 'end' : undefined} data-message-id={messageKey(message)}>
      <MessageContent className="gap-2">
        <MessageHeader className="gap-2 px-0 text-xs font-normal">
          <span>{roleLabel(message.role)}</span>
          {when ? (
            <time dateTime={when.toISOString()}>
              {when.toLocaleString('zh-CN', {
                month: 'numeric',
                day: 'numeric',
                hour: '2-digit',
                minute: '2-digit',
              })}
            </time>
          ) : null}
        </MessageHeader>
        <Bubble align={user ? 'end' : 'start'} variant={user ? 'muted' : 'ghost'}>
          <BubbleContent>{message.text}</BubbleContent>
        </Bubble>
        {message.blobs?.map((blob) => <Artifact key={blob.hash} hash={blob.hash} size={blob.size} />)}
      </MessageContent>
    </MessageRoot>
  )
}
