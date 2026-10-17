import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyTitle,
} from '@/components/ui/empty'
import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from '@/components/ui/message-scroller'

import { messageKey } from '../protocol.ts'
import type { TranscriptMessage } from '../types.ts'
import { Message } from './message.tsx'

export function Chat({
  messages,
  emptyTitle,
  emptyDetail,
}: {
  messages: TranscriptMessage[]
  emptyTitle: string
  emptyDetail: string
}) {
  if (messages.length === 0) {
    return (
      <Empty className="h-full">
        <EmptyHeader>
          <EmptyTitle>{emptyTitle}</EmptyTitle>
          <EmptyDescription>{emptyDetail}</EmptyDescription>
        </EmptyHeader>
      </Empty>
    )
  }

  return (
    <MessageScrollerProvider autoScroll>
      <MessageScroller>
        <MessageScrollerViewport>
          <MessageScrollerContent aria-label="聊天记录" className="gap-6">
            {messages.map((message) => (
              <MessageScrollerItem
                key={messageKey(message)}
                messageId={messageKey(message)}
                scrollAnchor={message.role === 'user' || undefined}
              >
                <Message message={message} />
              </MessageScrollerItem>
            ))}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        <MessageScrollerButton />
      </MessageScroller>
    </MessageScrollerProvider>
  )
}
