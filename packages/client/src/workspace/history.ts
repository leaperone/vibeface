import { byRank } from '../chat/timeline.ts'
import { mergeOwnerJobs, parseHistoryPage, withDeliveries } from './model.ts'
import type { ConversationPayload, DeliveryRecord, HistoryPage, HistorySection, ManagerJob, ConversationMemoryConflict } from './types.ts'

/** A retained row wins over a late page. A refresh may replace only rows unchanged since that read began. */
function mergeRows<T>(current: T[], incoming: T[], key: (item: T) => string, older: boolean, baseline?: T[], complete = false): T[] {
  const known = new Map(current.map((item) => [key(item), item]))
  const before = baseline && new Map(baseline.map((item) => [key(item), item]))
  const incomingByKey = new Map(incoming.map((item) => [key(item), item]))
  const updated = current.filter((item) => !complete || incomingByKey.has(key(item)) || before && before.get(key(item)) !== item).map((item) => {
    const next = incomingByKey.get(key(item))
    return next !== undefined && !older && (!before || before.get(key(item)) === item) ? next : item
  })
  const added = incoming.filter((item) => !known.has(key(item)))
  return older ? [...added, ...updated] : [...updated, ...added]
}
// Local and owner arrays are separate; a pending delivery gains its source when admitted.
const jobKey = (job: ManagerJob) => job.trigger
const deliveryKey = (item: DeliveryRecord) => JSON.stringify([item.work_id, item.state.startsWith('stop_') ? 'stop' : 'dispatch'])
const conflictKey = (item: ConversationMemoryConflict) => item.history_id
const syncConflictKey = (item: ConversationPayload['bot_settings']['memory_sync_conflicts']['items'][number]) => JSON.stringify([item.memory_id, item.losing_revision])
const byName = <T extends { name: string }>(items: T[], key: (item: T) => string) => items.sort((left, right) => left.name.toLocaleLowerCase().localeCompare(right.name.toLocaleLowerCase()) || key(left).localeCompare(key(right)))
const overwrittenKey = (item: ConversationPayload['overwritten'][number]) => JSON.stringify([item.change_id, item.field])
const sessionKey = (item: ConversationPayload['sessions'][number]) => JSON.stringify([item.node, item.session_id])
const scopeKey = (item: ConversationPayload['scopeJobs'][number]) => JSON.stringify([item.source_node, item.trigger])

export function mergeHistoryPage(shown: ConversationPayload, page: HistoryPage): ConversationPayload {
  if (shown.conversation?.conversation_id !== page.conversation_id) throw new Error('历史记录属于其他对话。')
  if (shown.history_authority !== page.history_authority) throw new Error('history changed; refresh the conversation')
  if (!shown.scheduler && (page.section === 'jobs' || page.section === 'scope_jobs' || page.section === 'unanswered' || page.section === 'owner_jobs' || page.section === 'owner_scope_jobs' || page.section === 'owner_memory_conflicts')) throw new Error('history changed; refresh the conversation')
  const next = { ...shown, history: { ...shown.history, [page.section]: { next_cursor: page.next_cursor, complete: page.complete } } }
  switch (page.section) {
    case 'messages': return { ...next, messages: byRank(mergeRows(shown.messages, page.items, (item) => item.message_id, true)) }
    case 'works': return { ...next, works: withDeliveries(mergeRows(shown.works.filter((item) => !item.state.startsWith('dispatch_')), page.items, (item) => item.work_id, true), shown.deliveries) }
    case 'changes': return { ...next, changes: mergeRows(shown.changes, page.items, (item) => item.change_id, true) }
    case 'questions': return { ...next, questions: mergeRows(shown.questions, page.items, (item) => item.question_id, true) }
    case 'sessions': return { ...next, sessions: byRank(mergeRows(shown.sessions, page.items, sessionKey, true)) }
    case 'checks': return { ...next, checks: mergeRows(shown.checks, page.items, (item) => item.check_id, true) }
    case 'scope_jobs': {
      const local = mergeRows(shown.scheduler?.scope_jobs ?? [], page.items, scopeKey, true)
      return { ...next, scopeJobs: mergeOwnerJobs(local, shown.scheduler?.remote?.scope_jobs ?? [], shown.scheduler?.remote?.reachable === true), scheduler: shown.scheduler && { ...shown.scheduler, scope_jobs: local } }
    }
    case 'jobs': {
      const local = mergeRows(shown.scheduler?.local_jobs ?? [], page.items, jobKey, true)
      return { ...next, scheduler: shown.scheduler && { ...shown.scheduler, local_jobs: local, jobs: mergeOwnerJobs(local, shown.scheduler.remote?.jobs ?? [], shown.scheduler.remote?.reachable === true) } }
    }
    case 'owner_jobs': {
      const remote = shown.scheduler?.remote
      if (!shown.scheduler || !remote) throw new Error('会话负责设备已变化，请刷新。')
      const jobs = mergeRows(remote.jobs, page.items, jobKey, true)
      return { ...next, scheduler: { ...shown.scheduler, remote: { ...remote, jobs }, jobs: mergeOwnerJobs(shown.scheduler.local_jobs, jobs, remote.reachable) } }
    }
    case 'owner_scope_jobs': {
      const remote = shown.scheduler?.remote
      if (!shown.scheduler || !remote) throw new Error('会话负责设备已变化，请刷新。')
      const jobs = mergeRows(remote.scope_jobs, page.items, scopeKey, true)
      return { ...next, scopeJobs: mergeOwnerJobs(shown.scheduler.scope_jobs, jobs, remote.reachable), scheduler: { ...shown.scheduler, remote: { ...remote, scope_jobs: jobs } } }
    }
    case 'owner_memory_conflicts': {
      const remote = shown.scheduler?.remote
      if (!shown.scheduler || !remote) throw new Error('会话负责设备已变化，请刷新。')
      return { ...next, scheduler: { ...shown.scheduler, remote: { ...remote, memory_conflicts: mergeRows(remote.memory_conflicts ?? [], page.items, conflictKey, true) } } }
    }
    case 'skills': return { ...next, bot_settings: { ...shown.bot_settings, skills: byName(mergeRows(shown.bot_settings.skills, page.items, (item) => item.skill_id, true), (item) => item.skill_id) } }
    case 'schedules': return { ...next, bot_settings: { ...shown.bot_settings, schedules: byName(mergeRows(shown.bot_settings.schedules, page.items, (item) => item.schedule_id, true), (item) => item.schedule_id) } }
    case 'memory': return { ...next, bot_settings: { ...shown.bot_settings, memory: mergeRows(shown.bot_settings.memory, page.items, (item) => item.memory_id, true) } }
    case 'memory_conflicts': return { ...next, bot_settings: { ...shown.bot_settings, memory_conflicts: mergeRows(shown.bot_settings.memory_conflicts ?? [], page.items, conflictKey, true) } }
    case 'memory_sync_conflicts': return { ...next, bot_settings: { ...shown.bot_settings, memory_sync_conflicts: { items: mergeRows(shown.bot_settings.memory_sync_conflicts.items, page.items, syncConflictKey, true), has_more: !page.complete } } }
    case 'overwritten': return { ...next, overwritten: mergeRows(shown.overwritten, page.items, overwrittenKey, true) }
    case 'unanswered': return { ...next, scheduler: shown.scheduler && { ...shown.scheduler, unanswered: mergeRows(shown.scheduler.unanswered, page.items, (item) => item, true) } }
    case 'deliveries': {
      const deliveries = mergeRows(shown.deliveries, page.items, deliveryKey, true)
      return { ...next, deliveries, works: withDeliveries(shown.works.filter((item) => !item.state.startsWith('dispatch_')), deliveries) }
    }
    default: { const exhaustive: never = page; return exhaustive }
  }
}

export function mergeConversationSnapshot(shown: ConversationPayload | null, incoming: ConversationPayload, baseline: ConversationPayload | null): ConversationPayload {
  if (!shown || shown.conversation?.conversation_id !== incoming.conversation?.conversation_id || shown.history_authority !== incoming.history_authority) return incoming
  const retained = shown
  const history = { ...incoming.history }
  function rows<T>(section: HistorySection, current: T[], items: T[], key: (item: T) => string, before?: T[]): T[] {
    // A disjoint newest window can leave a gap above loaded history. Resume from that new window in that case.
    const loaded = new Set(current.map(key))
    if (!incoming.history[section].complete && items.some((item) => loaded.has(key(item)))) history[section] = retained.history[section]
    return mergeRows(current, items, key, false, before, incoming.history[section].complete)
  }
  const deliveries = rows('deliveries', shown.deliveries, incoming.deliveries, deliveryKey, baseline?.deliveries)
  const localScopes = rows('scope_jobs', shown.scheduler?.scope_jobs ?? [], incoming.scheduler?.scope_jobs ?? [], scopeKey, baseline?.scheduler?.scope_jobs)
  const remote = incoming.scheduler?.remote
  const ownerScopes = rows('owner_scope_jobs', shown.scheduler?.remote?.scope_jobs ?? [], remote?.scope_jobs ?? [], scopeKey, baseline?.scheduler?.remote?.scope_jobs)
  const localJobs = rows('jobs', shown.scheduler?.local_jobs ?? [], incoming.scheduler?.local_jobs ?? [], jobKey, baseline?.scheduler?.local_jobs)
  const ownerJobs = rows('owner_jobs', shown.scheduler?.remote?.jobs ?? [], remote?.jobs ?? [], jobKey, baseline?.scheduler?.remote?.jobs)
  const ownerConflicts = rows('owner_memory_conflicts', shown.scheduler?.remote?.memory_conflicts ?? [], remote?.memory_conflicts ?? [], conflictKey, baseline?.scheduler?.remote?.memory_conflicts ?? undefined)
  return {
    ...incoming, history, deliveries, scopeJobs: mergeOwnerJobs(localScopes, ownerScopes, remote?.reachable === true),
    messages: byRank(rows('messages', shown.messages, incoming.messages, (item) => item.message_id, baseline?.messages)),
    works: withDeliveries(rows('works', shown.works, incoming.works, (item) => item.work_id, baseline?.works).filter((item) => !item.state.startsWith('dispatch_')), deliveries),
    changes: rows('changes', shown.changes, incoming.changes, (item) => item.change_id, baseline?.changes),
    questions: rows('questions', shown.questions, incoming.questions, (item) => item.question_id, baseline?.questions),
    sessions: byRank(rows('sessions', shown.sessions, incoming.sessions, sessionKey, baseline?.sessions)),
    checks: rows('checks', shown.checks, incoming.checks, (item) => item.check_id, baseline?.checks),
    overwritten: rows('overwritten', shown.overwritten, incoming.overwritten, overwrittenKey, baseline?.overwritten),
    bot_settings: { ...incoming.bot_settings,
      skills: byName(rows('skills', shown.bot_settings.skills, incoming.bot_settings.skills, (item) => item.skill_id, baseline?.bot_settings.skills), (item) => item.skill_id),
      schedules: byName(rows('schedules', shown.bot_settings.schedules, incoming.bot_settings.schedules, (item) => item.schedule_id, baseline?.bot_settings.schedules), (item) => item.schedule_id),
      memory: rows('memory', shown.bot_settings.memory, incoming.bot_settings.memory, (item) => item.memory_id, baseline?.bot_settings.memory),
      memory_conflicts: rows('memory_conflicts', shown.bot_settings.memory_conflicts ?? [], incoming.bot_settings.memory_conflicts ?? [], conflictKey, baseline?.bot_settings.memory_conflicts ?? undefined),
      memory_sync_conflicts: { ...incoming.bot_settings.memory_sync_conflicts, items: rows('memory_sync_conflicts', shown.bot_settings.memory_sync_conflicts.items, incoming.bot_settings.memory_sync_conflicts.items, syncConflictKey, baseline?.bot_settings.memory_sync_conflicts.items) },
    },
    scheduler: incoming.scheduler && { ...incoming.scheduler, local_jobs: localJobs, jobs: mergeOwnerJobs(localJobs, ownerJobs, remote?.reachable === true), scope_jobs: localScopes,
      unanswered: rows('unanswered', shown.scheduler?.unanswered ?? [], incoming.scheduler.unanswered, (item) => item, baseline?.scheduler?.unanswered),
      remote: remote && { ...remote, jobs: ownerJobs, scope_jobs: ownerScopes, memory_conflicts: ownerConflicts },
    },
  }
}

export const historyLabels: Record<HistorySection, string> = { messages: '聊天消息', works: '工作', changes: '设置变更', questions: '问题', sessions: '接手会话', jobs: '回复状态', scope_jobs: '任务状态', checks: '待确认记录', deliveries: '送达记录', unanswered: '未回复消息', owner_jobs: '负责设备的回复状态', owner_scope_jobs: '负责设备的任务状态', owner_memory_conflicts: '负责设备的记忆冲突', skills: '技能', schedules: '定时任务', memory: '记忆', memory_conflicts: '记忆冲突', memory_sync_conflicts: '记忆同步冲突', overwritten: '被覆盖的设置' }

/** User cancellation and a new visit invalidate all results from an older request. */
export class HistoryRequests {
  private revision = 0
  private pending = new Map<HistorySection, object>()
  reset() { this.revision += 1; this.pending.clear() }
  cancel(section: HistorySection) { this.pending.delete(section) }
  start(section: HistorySection) {
    if (this.pending.has(section)) return null
    const token = {}, revision = this.revision
    this.pending.set(section, token)
    return { current: () => this.revision === revision && this.pending.get(section) === token, finish: () => { if (this.pending.get(section) === token) this.pending.delete(section) } }
  }
}

/** Reads one visible page, advancing past bounded scans with no visible rows. */
export async function readConversationHistory(input: {
  rpc: (method: string, params: Record<string, unknown>) => Promise<unknown>
  conversationId: string; section: HistorySection; cursor: string | null; authority: string
  current: () => boolean; apply: (page: HistoryPage) => void
}): Promise<void> {
  let cursor = input.cursor
  while (input.current()) {
    const page = parseHistoryPage(await input.rpc('conversation_history', { conversation_id: input.conversationId, section: input.section, cursor, limit: 50 }), input.conversationId, input.section)
    if (!input.current()) return
    if (page.history_authority !== input.authority) throw new Error('history changed; refresh the conversation')
    if (page.next_cursor !== null && page.next_cursor === cursor) throw new Error('历史分页没有前进，请重新读取对话。')
    input.apply(page)
    cursor = page.next_cursor
    if (page.items.length > 0 || cursor === null) return
  }
}
