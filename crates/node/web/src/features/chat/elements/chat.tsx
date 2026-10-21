import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from '@/components/ui/message-scroller'

import type { ReactNode } from 'react'
import { Message, type ChatMessage } from './message'

/** One row of the conversation: a message bubble, or a task, change, question or hand-off row between them. */
export type ChatEntry =
  | { key: string; anchor?: boolean; message: ChatMessage; routine?: string }
  | { key: string; anchor?: undefined; node: ReactNode }

type Sender = 'user' | 'assistant'
const sender = (entry: ChatEntry | undefined): Sender | null => entry && 'message' in entry ? (entry.message.role === 'user' ? 'user' : 'assistant') : null
/** A routine run's reply stands alone, so it always opens a group. */
const continues = (entry: ChatEntry, previous: ChatEntry | undefined) => !('routine' in entry && entry.routine) && sender(entry) !== null && sender(entry) === sender(previous)

export function Chat({ entries, empty }: { entries: ChatEntry[]; empty: ReactNode }) {
  if (entries.length === 0) {
    return <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">{empty}</div>
  }

  // MessageScroller scrolls to the first anchored item it has not aligned yet, which after a remount is the oldest
  // message. Only the newest user message is an anchor so a send keeps the view at the bottom.
  const anchor = entries.map((entry) => entry.anchor === true).lastIndexOf(true)

  return (
    // The button shows only once the last line has scrolled out of view, so it never covers it.
    <MessageScrollerProvider autoScroll scrollEdgeThreshold={96}>
      <MessageScroller className="h-auto min-h-24 flex-1">
        <MessageScrollerViewport aria-label="聊天记录">
          <MessageScrollerContent className="mx-auto w-full max-w-3xl gap-0 px-4 py-6 sm:px-6 lg:px-8">
            {entries.map((entry, index) => {
              const first = !continues(entry, entries[index - 1])
              const next = entries[index + 1]
              const last = !next || !continues(next, entry)
              return (
                <MessageScrollerItem key={entry.key} messageId={entry.key} scrollAnchor={index === anchor || undefined} className={index === 0 ? undefined : first ? 'mt-5' : 'mt-1'}>
                  {'message' in entry ? <Message message={entry.message} routine={entry.routine} first={first} last={last} /> : entry.node}
                </MessageScrollerItem>
              )
            })}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        <MessageScrollerButton aria-label="回到最新消息" />
      </MessageScroller>
    </MessageScrollerProvider>
  )
}
