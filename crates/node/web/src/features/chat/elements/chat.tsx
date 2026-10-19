import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from '@/components/ui/message-scroller'

import type { ReactNode } from 'react'

/** One row of the conversation: a message, a task card, an answered approval or a card waiting on the person. */
export type ChatEntry = { key: string; anchor?: boolean; node: ReactNode }

export function Chat({ entries, empty }: { entries: ChatEntry[]; empty: ReactNode }) {
  if (entries.length === 0) {
    return <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">{empty}</div>
  }

  return (
    <MessageScrollerProvider autoScroll>
      <MessageScroller className="h-auto min-h-24 flex-1">
        <MessageScrollerViewport aria-label="聊天记录">
          <MessageScrollerContent className="mx-auto w-full max-w-3xl gap-6 px-4 py-6 sm:px-6 lg:px-8">
            {entries.map((entry) => (
              <MessageScrollerItem key={entry.key} messageId={entry.key} scrollAnchor={entry.anchor || undefined}>
                {entry.node}
              </MessageScrollerItem>
            ))}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        <MessageScrollerButton aria-label="回到最新消息" />
      </MessageScroller>
    </MessageScrollerProvider>
  )
}
