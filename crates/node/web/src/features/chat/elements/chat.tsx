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
import { Children, type ReactNode } from 'react'

export function Chat({
  messages,
  emptyTitle,
  emptyDetail,
  emptyMark,
  children,
}: {
  messages: TranscriptMessage[]
  emptyTitle: string
  emptyDetail: string
  emptyMark?: ReactNode
  children?: ReactNode
}) {
  if (messages.length === 0 && Children.toArray(children).length === 0) {
    return (
      <Empty className="min-h-0 overflow-y-auto px-4">
        <EmptyHeader>
          {emptyMark}
          <EmptyTitle>{emptyTitle}</EmptyTitle>
          <EmptyDescription>{emptyDetail}</EmptyDescription>
        </EmptyHeader>
      </Empty>
    )
  }

  return (
    <MessageScrollerProvider autoScroll>
      <MessageScroller className="h-auto min-h-24 flex-1">
        <MessageScrollerViewport aria-label="聊天记录">
          <MessageScrollerContent className="mx-auto w-full max-w-3xl gap-6 px-4 py-6 sm:px-6 lg:px-8">
            {messages.map((message) => (
              <MessageScrollerItem
                key={messageKey(message)}
                messageId={messageKey(message)}
                scrollAnchor={message.role === 'user' || undefined}
              >
                <Message message={message} />
              </MessageScrollerItem>
            ))}
            {children}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        <MessageScrollerButton aria-label="回到最新消息" />
      </MessageScroller>
    </MessageScrollerProvider>
  )
}
