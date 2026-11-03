import { placeByTime } from "../workspace/questions.ts"
import type { NotificationRecord } from "../workspace/types.ts"
import type { TranscriptMessage } from "./types.ts"

type Ordered = Pick<TranscriptMessage, "message_id" | "role" | "in_reply_to" | "created_at_ms" | "rank">

/** The Node's order, the same on every device; a message without a rank yet keeps its place after the ranked ones. */
export function byRank<T extends Pick<TranscriptMessage, "rank">>(messages: readonly T[]): T[] {
  return [...messages].sort((a, b) => a.rank === b.rank ? 0 : a.rank === undefined ? 1 : b.rank === undefined ? -1 : a.rank < b.rank ? -1 : 1)
}

/**
 * A message's first reply follows it, even when the person wrote again before that reply finished. A later reply to
 * the same message, such as a task's report, stays where it was written. A `joined` message went into the reply then
 * running, so it sits before that reply, after the message it answers.
 */
export function threadReplies<T extends Pick<TranscriptMessage, "message_id" | "role" | "in_reply_to">>(messages: readonly T[], joined: { has(id: string): boolean } = new Set()): T[] {
  const ids = new Set(messages.map((message) => message.message_id))
  const first = new Map<string, T>(), merged = new Map<string, T[]>(), moved = new Set<string>()
  let head: string | null = null
  for (const message of messages) {
    const to = message.role === "assistant" && message.in_reply_to !== undefined && ids.has(message.in_reply_to) ? message.in_reply_to : null
    if (to) { if (!first.has(to)) { first.set(to, message); moved.add(message.message_id) } }
    else if (head && joined.has(message.message_id)) { merged.set(head, [...merged.get(head) ?? [], message]); moved.add(message.message_id) }
    else head = message.message_id
  }
  const reply = (message: T): T[] => first.has(message.message_id) ? [first.get(message.message_id)!] : []
  return messages.flatMap((message) => moved.has(message.message_id) ? []
    : [message, ...(merged.get(message.message_id) ?? []).flatMap((item) => [item, ...reply(item)]), ...reply(message)])
}

/**
 * What a chat shows, in the order every device shows it: messages by rank with replies threaded, and each aside
 * (a question, hand-off or message to another bot) before the first message written after it, or in `rest` at the end.
 */
export function chatTimeline<M extends Ordered, A extends { created_at_ms: number }>(messages: readonly M[], asides: readonly A[] = [], joined: { has(id: string): boolean } = new Set()) {
  const ranked = byRank(messages)
  return { messages: threadReplies(ranked, joined), ...placeByTime(ranked, asides) }
}

const pad = (value: number) => String(value).padStart(2, "0")

/** Today the time only, this year the date too, older ones the year as well. */
export function messageTime(at: number, now: number): string {
  const date = new Date(at), today = new Date(now)
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`
  if (date.toDateString() === today.toDateString()) return clock
  if (date.getFullYear() === today.getFullYear()) return `${date.getMonth() + 1}月${date.getDate()}日 ${clock}`
  return `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()} ${clock}`
}

/** One time per minute: under the last of the messages that follow each other within it, in the order shown. */
export function messageTimes(messages: readonly Pick<TranscriptMessage, "message_id" | "created_at_ms">[], now: number): Map<string, string> {
  const times = new Map<string, string>()
  messages.forEach((message, index) => {
    const next = messages[index + 1]
    if (!next || Math.floor(next.created_at_ms / 60_000) !== Math.floor(message.created_at_ms / 60_000)) times.set(message.message_id, messageTime(message.created_at_ms, now))
  })
  return times
}

/**
 * Where 新消息 starts: the first, in the order shown, of the replies that were unread when the chat came on screen
 * (`fresh`, by event hash) and the asides still `waiting` on the person that were there then; one asked while the chat
 * is open is seen as it arrives.
 */
export function firstUnread<M extends Pick<TranscriptMessage, "message_id" | "event_hash">, A>(
  timeline: { messages: readonly M[]; before: ReadonlyMap<string, readonly A[]>; rest: readonly A[] }, fresh: ReadonlySet<string>, waiting: (aside: A) => boolean,
): M | A | undefined {
  for (const message of timeline.messages) {
    const aside = timeline.before.get(message.message_id)?.find(waiting)
    if (aside) return aside
    if (fresh.has(message.event_hash)) return message
  }
  return timeline.rest.find(waiting)
}

/** A bot's reply, or another bot's message, this device has not read; its notification ID is the hash of the event that wrote the message. */
export const unreadReply = (notification: Pick<NotificationRecord, "kind">) => ["manager_response", "manager_attention", "bot_message"].includes(notification.kind)

/**
 * What a chat's badge counts, per conversation: replies not read here and questions still waiting on the person.
 * A finished task reports through a reply and an asked question waits as itself, so their own notifications add nothing.
 */
export function unreadCounts(notifications: readonly NotificationRecord[], waiting: readonly { conversation_id: string }[]): Record<string, number> {
  const counts: Record<string, number> = {}
  for (const item of [...notifications.filter(unreadReply), ...waiting]) counts[item.conversation_id] = (counts[item.conversation_id] ?? 0) + 1
  return counts
}
