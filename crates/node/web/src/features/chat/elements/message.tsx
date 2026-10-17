import { messageKey, roleLabel } from '../protocol.ts'
import type { TranscriptMessage } from '../types.ts'

export function TranscriptMessageView({ message }: { message: TranscriptMessage }) {
  const user = message.role === 'user'
  const when = message.created_at_ms ? new Date(message.created_at_ms) : null
  return (
    <article
      className="chat-message"
      data-align={user ? 'end' : 'start'}
      data-message-id={messageKey(message)}
    >
      <header>
        <span>{roleLabel(message.role)}</span>
        {when ? <time dateTime={when.toISOString()}>{when.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })}</time> : null}
      </header>
      <div className="chat-bubble">{message.text}</div>
    </article>
  )
}
