import { withDeliveries } from "./model.ts"
import { questionOpen } from "./questions.ts"
import type { BotRecord, ConversationPayload, ConversationRecords, NotificationRecord, OpenWork, Question, WorkRecord } from "./types.ts"

const delivery = (item: WorkRecord) => item.state.startsWith("dispatch_")
const open = (item: WorkRecord) => !delivery(item) && (item.outcome === null || item.outcome === "unverified")

function upsert<T>(items: T[], changed: T[], key: (item: T) => string): T[] {
  const next = items.map((item) => changed.find((candidate) => key(candidate) === key(item)) ?? item)
  return [...next, ...changed.filter((item) => !items.some((candidate) => key(candidate) === key(item)))]
}

/** The open conversation with one pushed change merged in, in the shared message order. */
export function mergeConversation(shown: ConversationPayload, records: ConversationRecords): ConversationPayload {
  const messages = upsert(shown.messages, records.messages, (item) => item.message_id)
  if (messages.every((item) => item.rank)) messages.sort((a, b) => (a.rank! < b.rank! ? -1 : a.rank! > b.rank! ? 1 : 0))
  const works = upsert(shown.works.filter((item) => !delivery(item)).map(({ stop: _, ...item }) => item), records.works, (item) => item.work_id)
  // A push carries this device's jobs only; the owner's stay until the conversation is read again.
  const remote = shown.scheduler?.jobs.filter((job) => job.source_node) ?? []
  const jobs = [...records.jobs.filter((job) => job.state !== "pending_delivery" || !remote.some((item) => item.trigger === job.trigger)), ...remote]
  return {
    ...shown, messages, works: withDeliveries(works, records.deliveries), checks: records.checks, questions: records.questions, sent: records.sent,
    scheduler: shown.scheduler && { ...shown.scheduler, jobs, unanswered: records.unanswered },
  }
}

/** The network's open questions, with this conversation's replaced by what it holds now. */
export function mergeQuestions(questions: Question[], records: ConversationRecords): Question[] {
  return [...questions.filter((item) => item.conversation_id !== records.conversation_id), ...records.questions.filter(questionOpen)]
}

export function mergeNotifications(notifications: NotificationRecord[], records: ConversationRecords): NotificationRecord[] {
  return [...notifications.filter((item) => item.conversation_id !== records.conversation_id), ...records.notifications]
}

export function mergeBots(bots: BotRecord[], records: ConversationRecords): BotRecord[] {
  if (!records.last_message) return bots
  return bots.map((bot) => bot.conversation_id === records.conversation_id ? { ...bot, last_message: records.last_message } : bot)
}

/** Works no result has settled yet, across conversations: a settled one leaves the list. */
export function mergeOpenWorks(works: OpenWork[], records: ConversationRecords): OpenWork[] {
  const changed = records.works.map((item) => ({ ...item, conversation_id: records.conversation_id }))
  return upsert(works, changed, (item) => item.work_id).filter(open)
}
