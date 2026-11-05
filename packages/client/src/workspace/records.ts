import { byRank } from "../chat/timeline.ts"
import { mainMessage, mainQuestion, withDeliveries } from "./model.ts"
import { questionOpen } from "./questions.ts"
import type { BotRecord, ConversationPayload, ConversationRecords, NotificationRecord, OpenWork, Question, WorkRecord, WorkspaceCommandResult, WorkspaceDomainState } from "./types.ts"

const delivery = (item: WorkRecord) => item.state.startsWith("dispatch_")

function upsert<T>(items: T[], changed: T[], key: (item: T) => string): T[] {
  const next = items.map((item) => changed.find((candidate) => key(candidate) === key(item)) ?? item)
  return [...next, ...changed.filter((item) => !items.some((candidate) => key(candidate) === key(item)))]
}

export function mergeWorkspaceState(shown: WorkspaceDomainState | null, changed: WorkspaceDomainState | WorkspaceCommandResult, scoped = false): WorkspaceDomainState {
  const current = shown ?? { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 0, truncated: true } }
  return {
    channels: upsert(current.channels, changed.channels.filter((item) => !current.channels.some((known) => known.channel_id === item.channel_id && known.revision > item.revision)), (item) => item.channel_id),
    tasks: upsert(current.tasks, changed.tasks.filter((item) => !current.tasks.some((known) => known.task_id === item.task_id && known.revision > item.revision)), (item) => item.task_id),
    assignments: upsert(current.assignments, changed.assignments, (item) => item.assignment_id),
    inputs: upsert(current.inputs, changed.inputs, (item) => item.input.message_id),
    coverage: 'coverage' in changed && !scoped ? changed.coverage : current.coverage,
  }
}

/** The open conversation with one pushed change merged in, in the shared message order. */
export function mergeConversation(shown: ConversationPayload, records: ConversationRecords): ConversationPayload {
  if (shown.conversation?.conversation_id !== records.conversation_id) throw new Error('推送记录属于别的对话。')
  const messages = byRank(upsert(shown.messages.filter(mainMessage), records.messages.filter(mainMessage), (item) => item.message_id))
  const works = upsert(shown.works.filter((item) => !delivery(item)).map(({ stop: _, ...item }) => item), records.works, (item) => item.work_id)
  // A push carries this device's jobs only; the owner's stay until the conversation is read again.
  const remote = shown.scheduler?.jobs.filter((job) => job.source_node && job.source_node !== shown.local_node) ?? []
  const jobs = [...records.jobs.filter((job) => job.scope.kind === 'main' && (job.state !== "pending_delivery" || !remote.some((item) => item.trigger === job.trigger))), ...remote]
  const remoteScopes = shown.scopeJobs.filter((job) => job.source_node !== shown.local_node && !records.scope_jobs.some((item) => item.trigger === job.trigger))
  const scopeJobs = [...records.scope_jobs, ...remoteScopes]
  return {
    ...shown, messages, scopeJobs, works: withDeliveries(works, records.deliveries), checks: records.checks, questions: records.questions.filter(mainQuestion), sent: records.sent, sent_coverage: records.sent_coverage,
    scheduler: shown.scheduler && { ...shown.scheduler, jobs, scope_jobs: scopeJobs, unanswered: records.unanswered,
      ...(!shown.scheduler.remote && records.context !== undefined && (records.context === null || records.context.scope.kind === 'main') ? { context: records.context } : {}),
    },
    bot_settings: { ...shown.bot_settings,
      ...(records.memory === undefined ? {} : { memory: records.memory }),
      ...(records.memory_sync_conflicts === undefined ? {} : { memory_sync_conflicts: records.memory_sync_conflicts }),
      ...(!shown.scheduler?.remote && records.memory_staged_count !== undefined ? { memory_staged_count: records.memory_staged_count } : {}),
      ...(!shown.scheduler?.remote && records.memory_conflicts !== undefined ? { memory_conflicts: records.memory_conflicts } : {}),
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
