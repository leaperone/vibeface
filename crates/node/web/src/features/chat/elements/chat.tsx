import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from '@/components/ui/message-scroller'

import { useRef, type ReactNode } from 'react'
import { Message, type ChatMessage } from './message'

/** One row of the conversation: a message bubble, or a task, change, question or hand-off row between them. */
export type ChatEntry =
  | { key: string; anchor?: boolean; message: ChatMessage; schedule?: string; retry?: () => Promise<void> }
  | { key: string; anchor?: undefined; node: ReactNode }

/** `user`, `assistant`, or `bot:<conversation_id>` for another bot's message. */
const sender = (entry: ChatEntry | undefined): string | null => {
  if (!entry || !('message' in entry)) return null
  const { role, sender } = entry.message
  return role === 'user' ? 'user' : role === 'bot' ? `bot:${sender?.conversation_id ?? ''}` : 'assistant'
}
/** A schedule run's reply stands alone, so it always opens a group. The time shows once, at the end of a group. */
const continues = (entry: ChatEntry, previous: ChatEntry | undefined) => !('schedule' in entry && entry.schedule) && sender(entry) !== null && sender(entry) === sender(previous)

export function Chat({ entries, empty }: { entries: ChatEntry[]; empty: ReactNode }) {
  // MessageScroller scrolls to the first anchored item it has not aligned yet. Only a user message sent after the
  // chat opened is an anchor, so opening a chat stays at its newest entry and a send still lands at the bottom.
  const newest = entries.map((entry) => entry.anchor === true).lastIndexOf(true)
  const opened = useRef<string | null | undefined>(undefined)
  const newestKey = newest >= 0 ? entries[newest].key : null
  if (opened.current === undefined && entries.length > 0) opened.current = newestKey
  const anchor = opened.current !== undefined && newestKey !== opened.current ? newest : -1

  if (entries.length === 0) {
    return <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">{empty}</div>
  }

  return (
    // The button shows only once the last line has scrolled out of view, so it never covers it.
    <MessageScrollerProvider autoScroll scrollEdgeThreshold={96}>
      <MessageScroller className="h-auto min-h-24 flex-1">
        <MessageScrollerViewport aria-label="聊天记录">
          <MessageScrollerContent className="mx-auto w-full max-w-(--page-width) gap-0 px-4 py-6 sm:px-6 lg:px-8">
            {entries.map((entry, index) => {
              const next = entries[index + 1]
              const last = !next || !continues(next, entry)
              return (
                <MessageScrollerItem key={entry.key} messageId={entry.key} scrollAnchor={index === anchor || undefined} className={index === 0 ? undefined : 'mt-6'}>
                  {'message' in entry ? <Message message={entry.message} schedule={entry.schedule} last={last} retry={entry.retry} /> : entry.node}
                </MessageScrollerItem>
              )
            })}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        {/* Sits at the composer's end, level with the waiting pill at its start, so the two never crowd each other. */}
        <MessageScrollerButton aria-label="回到最新消息" title="回到最新" className="size-8 inset-s-auto translate-x-0 rounded-full shadow-sm rtl:translate-x-0 data-[direction=end]:bottom-0 inset-e-[max(1rem,calc(50%-23rem))] max-md:size-10 sm:inset-e-[max(1.5rem,calc(50%-22.5rem))] lg:inset-e-[max(2rem,calc(50%-22rem))]" />
      </MessageScroller>
    </MessageScrollerProvider>
  )
}
