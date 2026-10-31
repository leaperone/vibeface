import { byRank } from "../chat/timeline.ts"
import { withDeliveries } from "./model.ts"
import { questionOpen } from "./questions.ts"
import type { BotRecord, ConversationPayload, ConversationRecords, NotificationRecord, OpenWork, Question, WorkRecord } from "./types.ts"

const delivery = (item: WorkRecord) => item.state.startsWith("dispatch_")

function upsert<T>(items: T[], changed: T[], key: (item: T) => string): T[] {
  const next = items.map((item) => changed.find((candidate) => key(candidate) === key(item)) ?? item)
  return [...next, ...changed.filter((item) => !items.some((candidate) => key(candidate) === key(item)))]
}

/** The open conversation with one pushed change merged in, in the shared message order. */
export function mergeConversation(shown: ConversationPayload, records: ConversationRecords): ConversationPayload {
  const messages = byRank(upsert(shown.messages, records.messages, (item) => item.message_id))
  const works = upsert(shown.works.filter((item) => !delivery(item)).map(({ stop: _, ...item }) => item), records.works, (item) => item.work_id)
  // A push carries this device's jobs only; the owner's stay until the conversation is read again.
  const remote = shown.scheduler?.jobs.filter((job) => job.source_node) ?? []
  const jobs = [...records.jobs.filter((job) => job.state !== "pending_delivery" || !remote.some((item) => item.trigger === job.trigger)), ...remote]
  return {
    ...shown, messages, works: withDeliveries(works, records.deliveries), checks: records.checks, questions: records.questions, sent: records.sent,
    scheduler: shown.scheduler && { ...shown.scheduler, jobs, unanswered: records.unanswered,
      ...(!shown.scheduler.remote && records.context !== undefined ? { context: records.context } : {}),
    },
    bot_settings: { ...shown.bot_settings,
      ...(records.memory_conflicts === undefined ? {} : { memory_conflicts: records.memory_conflicts }),
    },
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
  return bots.map((bot) => bot.conversation_id === records.conversation_id ? { ...bot, last_message: records.last_message ?? bot.last_message, busy: records.busy } : bot)
}

/** The network's works that want the person, with this conversation's share replaced by what the Node lists now. */
export function mergeOpenWorks(works: OpenWork[], records: ConversationRecords): OpenWork[] {
  return [...works.filter((item) => item.conversation_id !== records.conversation_id), ...records.open_works]
}
