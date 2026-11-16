import { byRank } from "../chat/timeline.ts"
import { mainMessage, withDeliveries } from "./model.ts"
import type { ConversationPayload, ConversationRecords, WorkRecord, WorkspaceCommandResult, WorkspaceDomainState } from "./types.ts"

const delivery = (item: WorkRecord) => item.state.startsWith("dispatch_")

function upsert<T>(items: T[], changed: T[], key: (item: T) => string): T[] {
  const firstChanged = new Map<string, T>(), existing = new Set(items.map(key))
  for (const item of changed) {
    const id = key(item)
    if (!firstChanged.has(id)) firstChanged.set(id, item)
  }
  const next = items.map((item) => firstChanged.get(key(item)) ?? item)
  return [...next, ...changed.filter((item) => !existing.has(key(item)))]
}

function withoutStaleRevisions<T extends { revision: number }>(items: T[], changed: T[], key: (item: T) => string): T[] {
  const revisions = new Map<string, number>()
  for (const item of items) {
    const id = key(item)
    if (item.revision > (revisions.get(id) ?? -Infinity)) revisions.set(id, item.revision)
  }
  return changed.filter((item) => !((revisions.get(key(item)) ?? -Infinity) > item.revision))
}

export function mergeWorkspaceState(shown: WorkspaceDomainState | null, changed: WorkspaceDomainState | WorkspaceCommandResult, scoped = false): WorkspaceDomainState {
  const current = shown ?? { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 0, truncated: true } }
  return {
    channels: upsert(current.channels, withoutStaleRevisions(current.channels, changed.channels, (item) => item.channel_id), (item) => item.channel_id),
    tasks: upsert(current.tasks, withoutStaleRevisions(current.tasks, changed.tasks, (item) => item.task_id), (item) => item.task_id),
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
