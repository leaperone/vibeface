import type { WorkspaceHistory, WorkspaceHistoryItem } from '@vibeface/client/workspace/types.ts'

/** Inputs already carry the server's total event order; wall-clock timestamps cannot sort them. */
function appendUnique(older: WorkspaceHistoryItem[], newer: WorkspaceHistoryItem[]) {
  const incoming = new Set(newer.map(event => event.event_hash))
  return [...older.filter(event => !incoming.has(event.event_hash)), ...newer]
}

export function prependWorkspaceHistory(current: WorkspaceHistory, older: WorkspaceHistory): WorkspaceHistory {
  return { ...older, events: appendUnique(older.events, current.events) }
}

/** Bridge a busy interval before joining the newest page to the already loaded older window. */
export async function refreshWorkspaceHistory({ current, first, readOlder, isCurrent }: {
  current: WorkspaceHistory | null
  first: WorkspaceHistory
  readOlder(cursor: string): Promise<WorkspaceHistory>
  isCurrent(): boolean
}): Promise<WorkspaceHistory | null> {
  let fresh = first
  const known = new Set(current?.events.map(event => event.event_hash) ?? [])
  const visited = new Set<string>()
  while (current && !fresh.complete && !fresh.events.some(event => known.has(event.event_hash))) {
    if (!isCurrent()) return null
    const cursor = fresh.next_cursor
    if (!cursor || visited.has(cursor)) throw new Error('历史分页未能继续，请重新加载记录。')
    visited.add(cursor)
    fresh = prependWorkspaceHistory(fresh, await readOlder(cursor))
  }
  if (!isCurrent()) return null
  if (!current || fresh.complete) return fresh
  return { ...current, events: appendUnique(current.events, fresh.events) }
}

/** Search destinations are real message IDs (or an explicit event hash), never a work/task ID. */
export function workspaceHistoryMatchesMessage(event: WorkspaceHistoryItem, messageId: string) {
  if (!messageId) return false
  const body = event.body
  return event.event_hash === messageId || ('message_id' in body && body.message_id === messageId) || (body.type === 'workspace_commanded' && body.command.kind === 'post_channel_message' && body.command.message_id === messageId)
}

/** Persistent scope locks also cover leaving and reopening the same composer during preparation. */
export function createWorkspaceSendActivity() {
  const activities = new Map<string, { phase: 'preparing' | 'saving'; token: object }>()
  const listeners = new Set<() => void>()
  const changed = () => { for (const listener of listeners) listener() }
  return {
    subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener) } },
    phase(scope: string) { return activities.get(scope)?.phase ?? null },
    acquire(scope: string) {
      if (activities.has(scope)) return null
      const token = {}
      activities.set(scope, { phase: 'preparing', token }); changed()
      return {
        saving() { if (activities.get(scope)?.token === token) { activities.set(scope, { phase: 'saving', token }); changed() } },
        release() { if (activities.get(scope)?.token === token) { activities.delete(scope); changed() } },
      }
    },
  }
}
