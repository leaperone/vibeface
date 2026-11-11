import { byRank } from "../chat/timeline.ts"
import { mainMessage, withDeliveries } from "./model.ts"
import type { ConversationPayload, ConversationRecords, WorkRecord, WorkspaceCommandResult, WorkspaceDomainState } from "./types.ts"

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
  if (shown.history_authority !== records.history_authority) throw new Error("history changed; refresh the conversation")
  if (shown.conversation?.conversation_id !== records.conversation_id) throw new Error('推送记录属于别的对话。')
  const messages = byRank(upsert(shown.messages.filter(mainMessage), records.messages.filter(mainMessage), (item) => item.message_id))
  const works = upsert(shown.works.filter((item) => !delivery(item)), records.works, (item) => item.work_id)
  return { ...shown, messages, works: withDeliveries(works, shown.deliveries) }
}
