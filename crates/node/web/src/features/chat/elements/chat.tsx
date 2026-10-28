import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from '@/components/ui/message-scroller'

import { useRef, type ReactNode } from 'react'
import { cn } from '@/lib/utils'
import { Message, type ChatMessage } from './message'

/** One row of the conversation: a message bubble, or a task, change, question or hand-off row between them. */
export type ChatEntry =
  | { key: string; anchor?: boolean; message: ChatMessage; schedule?: string; retry?: () => Promise<void>; detail?: string }
  | { key: string; anchor?: undefined; node: ReactNode }

/** `user`, `assistant`, or `bot:<conversation_id>` for another bot's message. */
const sender = (entry: ChatEntry | undefined): string | null => {
  if (!entry || !('message' in entry)) return null
  const { role, sender } = entry.message
  return role === 'user' ? 'user' : role === 'bot' ? `bot:${sender?.conversation_id ?? ''}` : 'assistant'
}
/** A schedule run's reply stands alone, so it always opens a group. The time shows once, at the end of a group. */
const continues = (entry: ChatEntry, previous: ChatEntry | undefined) => !('schedule' in entry && entry.schedule) && sender(entry) !== null && sender(entry) === sender(previous)

/** Tasks one turn started stack as a group instead of spacing out like separate turns. */
const stacked = (entry: ChatEntry, previous: ChatEntry) => entry.key.startsWith('task:') && previous.key.startsWith('task:')

export function Chat({ entries, empty }: { entries: ChatEntry[]; empty: ReactNode }) {
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

  if (entries.length === 0) {
    return <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">{empty}</div>
  }

  return (
    // The button shows only once the last line has scrolled out of view, so it never covers it.
    <MessageScrollerProvider autoScroll scrollEdgeThreshold={96}>
      <MessageScroller className="h-auto min-h-24 flex-1">
        <MessageScrollerViewport aria-label="聊天记录">
          {/* Room for a toast over the composer, so the last card can scroll clear of it. */}
          <MessageScrollerContent className="mx-auto w-full max-w-(--page-width) gap-0 px-4 pt-6 pb-[calc(1.5rem+var(--toast-cover,0px))] sm:px-6">
            {entries.map((entry, index) => {
              const next = entries[index + 1]
              const last = !next || !continues(next, entry)
              return (
                <MessageScrollerItem key={entry.key} messageId={entry.key} scrollAnchor={index === anchor || undefined} className={cn(index > 0 && (stacked(entry, entries[index - 1]) ? 'mt-2' : 'mt-6'), entered.current.has(entry.key) && 'message-enter')}>
                  {'message' in entry ? <Message message={entry.message} schedule={entry.schedule} last={last} retry={entry.retry} detail={entry.detail} /> : entry.node}
                </MessageScrollerItem>
              )
            })}
          </MessageScrollerContent>
        </MessageScrollerViewport>
        {/* Sits at the composer's end, level with the waiting pill at its start, so the two never crowd each other. */}
        <MessageScrollerButton behavior={typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth'} aria-label="回到最新消息" title="回到最新" className="size-8 inset-s-auto translate-x-0 rounded-full shadow-sm rtl:translate-x-0 data-[direction=end]:bottom-0 inset-e-[max(1rem,calc(50%-23rem))] max-md:size-11 sm:inset-e-[max(1.5rem,calc(50%-22.5rem))]" />
      </MessageScroller>
    </MessageScrollerProvider>
  )
}
