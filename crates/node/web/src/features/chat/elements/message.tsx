import { Bubble, BubbleContent } from '@/components/ui/bubble'
import { Message as MessageRoot, MessageContent } from '@/components/ui/message'

import { messageKey, roleLabel } from '../protocol.ts'
import type { TranscriptMessage } from '../types.ts'

export function Message({ message }: { message: TranscriptMessage }) {
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  return (
    <MessageRoot align={user ? 'end' : undefined} data-message-id={messageKey(message)}>
      <MessageContent className="gap-2">
        <header>
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
        </header>
        {user ? (
          <Bubble align="end" variant="muted">
            <BubbleContent>{message.text}</BubbleContent>
          </Bubble>
        ) : (
          <p className="whitespace-pre-wrap">{message.text}</p>
        )}
      </MessageContent>
    </MessageRoot>
  )
}
