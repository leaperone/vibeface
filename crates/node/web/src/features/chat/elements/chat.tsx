import { useEffect, useRef, useState } from 'react'

import type { TranscriptMessage } from '../types.ts'
import { TranscriptMessageView } from './message.tsx'

export function ConversationThread({
  messages,
  emptyTitle,
  emptyDetail,
}: {
  messages: TranscriptMessage[]
  emptyTitle: string
  emptyDetail: string
}) {
  const viewportRef = useRef<HTMLDivElement>(null)
  const [away, setAway] = useState(false)

  useEffect(() => {
    const viewport = viewportRef.current
    if (!viewport || messages.length === 0) return
    const lastUser = [...viewport.querySelectorAll<HTMLElement>('[data-align="end"]')].at(-1)
    if (messages.at(-1)?.role === 'user' && lastUser) {
      lastUser.scrollIntoView({ block: 'start' })
    } else {
      viewport.scrollTop = viewport.scrollHeight
    }
  }, [messages])

  function onScroll() {
    const viewport = viewportRef.current
    if (!viewport) return
    const distance = viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight
    setAway(distance > 48)
  }

  return (
    <div className="chat-thread">
      <div ref={viewportRef} className="chat-viewport" onScroll={onScroll}>
        {messages.length === 0 ? (
          <div className="chat-empty">
            <strong>{emptyTitle}</strong>
            <p>{emptyDetail}</p>
          </div>
        ) : (
          <div className="chat-log" role="log" aria-label="聊天记录" aria-live="polite" aria-relevant="additions">
            {messages.map((message) => {
              const user = message.role === 'user'
              return (
                <div key={message.event_hash || message.message_id} data-scroll-anchor={user ? 'true' : undefined}>
                  <TranscriptMessageView message={message} />
                </div>
              )
            })}
          </div>
        )}
      </div>
      {away ? (
        <button
          type="button"
          className="chat-jump"
          onClick={() => {
            const viewport = viewportRef.current
            if (!viewport) return
            viewport.scrollTop = viewport.scrollHeight
          }}
        >
          回到底部
        </button>
      ) : null}
    </div>
  )
}
