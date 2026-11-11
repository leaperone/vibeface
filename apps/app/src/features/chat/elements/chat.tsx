import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from '@/components/ui/message-scroller'
import { useMessageScroller } from '@shadcn/react/message-scroller'

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { messageTimes } from '@vibeface/client/chat/timeline.ts'
import { cn } from '@/lib/utils'
import { Message, type ChatMessage } from './message'

/** One row of the conversation: a message bubble, or a task, change, question or hand-off row between them. */
export type ChatEntry =
  | { key: string; anchor?: boolean; message: ChatMessage; conversationId?: string; schedule?: string; retry?: () => Promise<void>; detail?: string }
  | { key: string; anchor?: undefined; node: ReactNode }

/** Opening a chat with 新消息 brings it into view. */
function ToUnread({ shown }: { shown: boolean }) {
  const { scrollToMessage } = useMessageScroller()
  useEffect(() => { if (shown) scrollToMessage('unread', { align: 'nearest' }) }, [shown, scrollToMessage])
  return null
}

export function Chat({ entries, empty }: { entries: ChatEntry[]; empty: ReactNode }) {
  const viewport = useRef<HTMLDivElement>(null)
  const visibleAnchor = useRef<{ key: string; offset: number } | null>(null)
  const [reading, setReading] = useState(false)
  const captureAnchor = () => {
    const view = viewport.current
    if (!view) return
    const away = view.scrollHeight - view.clientHeight - view.scrollTop > 96
    setReading(away)
    if (!away) { visibleAnchor.current = null; return }
    const top = view.getBoundingClientRect().top
    const row = [...view.querySelectorAll<HTMLElement>('[data-history-key]')].find((item) => item.dataset.historyKey !== 'history-controls' && item.getBoundingClientRect().bottom > top)
    if (row?.dataset.historyKey) visibleAnchor.current = { key: row.dataset.historyKey, offset: row.getBoundingClientRect().top - top }
  }
  const restoreAnchor = () => {
    const view = viewport.current, anchor = visibleAnchor.current
    if (!view || !anchor) return
    const row = [...view.querySelectorAll<HTMLElement>('[data-history-key]')].find((item) => item.dataset.historyKey === anchor.key)
    if (row) view.scrollTop += row.getBoundingClientRect().top - view.getBoundingClientRect().top - anchor.offset
  }
  useLayoutEffect(() => { restoreAnchor() }, [entries])
  useEffect(() => {
    const view = viewport.current
    if (!view) return
    const content = view.querySelector('[data-slot="message-scroller-content"]')
    const observer = new ResizeObserver(restoreAnchor)
    if (content) observer.observe(content)
    return () => observer.disconnect()
  }, [entries.length === 0])
  // MessageScroller scrolls to the first anchored item it has not aligned yet. Only a user message sent after the
  // chat opened is an anchor, so opening a chat stays at its newest entry and a send still lands at the bottom.
  const newest = entries.map((entry) => entry.anchor === true).lastIndexOf(true)
  const opened = useRef<string | null | undefined>(undefined)
  const newestKey = newest >= 0 ? entries[newest].key : null
  if (opened.current === undefined && entries.length > 0) opened.current = newestKey
  const anchor = opened.current !== undefined && newestKey !== opened.current ? newest : -1
  // Messages already here when the chat opened stay still; one that arrives later eases in once, as it mounts.
  // A finished reply takes over from its streamed text under a new key, so it does not ease in a second time.
  const seen = useRef<Set<string> | null>(null)
  const entered = useRef(new Set<string>())
  const streamed = useRef<string[]>([])
  seen.current ??= new Set(entries.map((entry) => entry.key))
  const keys = new Set(entries.map((entry) => entry.key))
  let handedOff = streamed.current.filter((key) => !keys.has(key)).length
  for (const entry of entries) {
    if (seen.current.has(entry.key)) continue
    seen.current.add(entry.key)
    if (!('message' in entry)) continue
    if (entry.message.role !== 'user' && entry.message.state !== 'streaming' && handedOff > 0) handedOff -= 1
    else entered.current.add(entry.key)
  }
  streamed.current = entries.filter((entry) => 'message' in entry && entry.message.state === 'streaming').map((entry) => entry.key)

  const times = messageTimes(entries.flatMap((entry) => 'message' in entry && entry.message.created_at_ms ? [entry.message] : []), Date.now())

  if (entries.length === 0) {
    return <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">{empty}</div>
  }

  return (
    // The button shows only once the last line has scrolled out of view, so it never covers it.
    <MessageScrollerProvider autoScroll={!reading} scrollEdgeThreshold={96}>
      <MessageScroller className="h-auto min-h-24 flex-1">
        <MessageScrollerViewport ref={viewport} onScroll={captureAnchor} aria-label="聊天记录">
          {/* Room for a toast over the composer, so the last card can scroll clear of it. */}
          <MessageScrollerContent className="mx-auto w-full max-w-(--page-width) gap-0 px-4 pt-6 pb-[calc(1.5rem+var(--toast-cover,0px))] sm:px-6">
            {entries.map((entry, index) => (
              <MessageScrollerItem data-history-key={entry.key} key={entry.key} messageId={entry.key} scrollAnchor={!reading && index === anchor || undefined} className={cn(index > 0 && 'mt-6', entered.current.has(entry.key) && 'message-enter')}>
                {'message' in entry ? <Message message={entry.message} conversationId={entry.conversationId} schedule={entry.schedule} time={times.get(entry.message.message_id)} retry={entry.retry} detail={entry.detail} /> : entry.node}
              </MessageScrollerItem>
            ))}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        {/* Sits at the composer's end, level with the waiting pill at its start, so the two never crowd each other. */}
        <MessageScrollerButton behavior={typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth'} aria-label="回到最新消息" title="回到最新" className="size-8 inset-s-auto translate-x-0 rounded-full shadow-sm rtl:translate-x-0 data-[direction=end]:bottom-0 inset-e-[max(1rem,calc(50%-23rem))] max-md:size-11 sm:inset-e-[max(1.5rem,calc(50%-22.5rem))]" />
      </MessageScroller>
      <ToUnread shown={entries.some((entry) => entry.key === 'unread')} />
    </MessageScrollerProvider>
  )
}
