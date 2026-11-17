import type { Platform } from '@vibeface/client/lib/platform.ts'
import { parseWorkspaceHistory } from '@vibeface/client/workspace/model.ts'
import type { MessageSearchItem, WorkspaceHistory, WorkspaceHistoryItem } from '@vibeface/client/workspace/types.ts'

export type WorkspaceHistoryCacheScope = { networkId: string; deviceId: string; target: { kind: 'task' | 'channel'; id: string } }
export type CachedWorkspaceHistory = WorkspaceHistoryCacheScope & {
  savedAt: number; title: string; botIds: string[]; events: WorkspaceHistoryItem[]; complete: boolean
}
export type CachedWorkspaceSearchItem = Pick<MessageSearchItem, 'event_hash' | 'message_id' | 'conversation_id' | 'text' | 'created_at_ms' | 'speaker'>
type StoredHistory = WorkspaceHistoryCacheScope & { savedAt: number; title: string; botIds: string[]; pages: WorkspaceHistory[]; retainedAll: boolean }
const storageKey = 'workspace-history-cache'
const bytes = (value: string) => new TextEncoder().encode(value).length
const scopeKey = (scope: WorkspaceHistoryCacheScope) => JSON.stringify([scope.networkId, scope.deviceId, scope.target.kind, scope.target.id])
const validScope = (scope: WorkspaceHistoryCacheScope) => Boolean(scope.networkId && scope.deviceId && scope.target.id)

/** Cache records are read-only display data. Persisted cursors never authorize pagination after reconnect. */
export function createWorkspaceHistoryCache({ storage, now = Date.now, maxBytes = 8 * 1024 * 1024, maxScopes = 12, maxEvents = 2048 }: {
  storage: Platform['storage']; now?: () => number; maxBytes?: number; maxScopes?: number; maxEvents?: number
}) {
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 2 || !Number.isSafeInteger(maxScopes) || maxScopes < 1 || !Number.isSafeInteger(maxEvents) || maxEvents < 1) throw new Error('本地历史缓存容量无效。')
  let previousRaw: string | null | undefined
  let previousRecords: StoredHistory[] = []
  function parse(value: unknown): StoredHistory | null {
    if (!value || typeof value !== 'object' || !('networkId' in value) || typeof value.networkId !== 'string' || !('deviceId' in value) || typeof value.deviceId !== 'string' || !('target' in value) || !value.target || typeof value.target !== 'object' || !('kind' in value.target) || (value.target.kind !== 'task' && value.target.kind !== 'channel') || !('id' in value.target) || typeof value.target.id !== 'string') return null
    const scope: WorkspaceHistoryCacheScope = { networkId: value.networkId, deviceId: value.deviceId, target: { kind: value.target.kind, id: value.target.id } }
    if (!validScope(scope) || !('savedAt' in value) || typeof value.savedAt !== 'number' || !Number.isSafeInteger(value.savedAt) || value.savedAt < 0 || !('title' in value) || typeof value.title !== 'string' || !('botIds' in value) || !Array.isArray(value.botIds) || !('pages' in value) || !Array.isArray(value.pages) || !value.pages.length || !('retainedAll' in value) || typeof value.retainedAll !== 'boolean') return null
    const botIds: string[] = []
    for (const id of value.botIds) { if (typeof id !== 'string' || !id) return null; botIds.push(id) }
    const pages: WorkspaceHistory[] = value.pages.map(parseWorkspaceHistory)
    if (pages.some(page => page.coverage.scope !== scope.target.kind || page.history_authority !== pages[0].history_authority)) return null
    const events = pages.flatMap(page => page.events)
    if (events.length > maxEvents || new Set(events.map(event => event.event_hash)).size !== events.length) return null
    return { ...scope, savedAt: value.savedAt, title: value.title, botIds, pages, retainedAll: value.retainedAll }
  }
  function all() {
    try {
      const raw = storage.getItem(storageKey)
      if (raw === previousRaw) return previousRecords
      previousRaw = raw; previousRecords = []
      if (!raw || bytes(raw) > maxBytes) return []
      const values: unknown = JSON.parse(raw)
      if (!Array.isArray(values) || values.length > maxScopes) return []
      previousRecords = values.flatMap(value => { try { const record = parse(value); return record ? [record] : [] } catch { return [] } })
      return previousRecords
    } catch { return [] }
  }
  function display(record: StoredHistory): CachedWorkspaceHistory {
    return { networkId: record.networkId, deviceId: record.deviceId, target: record.target, savedAt: record.savedAt, title: record.title, botIds: record.botIds, events: record.pages.flatMap(page => page.events), complete: record.retainedAll && record.pages.every(page => page.complete) }
  }
  function read(scope: WorkspaceHistoryCacheScope): CachedWorkspaceHistory | null {
    if (!validScope(scope)) return null
    const record = all().find(record => scopeKey(record) === scopeKey(scope))
    if (!record) return null
    return display(record)
  }
  function write(scope: WorkspaceHistoryCacheScope, input: { history: WorkspaceHistory; title: string; botIds: string[] }) {
    if (!validScope(scope) || input.history.coverage.scope !== scope.target.kind) return false
    let events = input.history.events.slice(-maxEvents)
    let retainedAll = events.length === input.history.events.length
    const makeRecord = (): StoredHistory => {
      const pages: WorkspaceHistory[] = []
      for (let offset = 0; offset < events.length || !pages.length; offset += 512) pages.push(parseWorkspaceHistory({ ...input.history, events: events.slice(offset, offset + 512), coverage: { ...input.history.coverage, limit: 512 } }))
      return { ...scope, savedAt: now(), title: input.title, botIds: input.botIds, pages, retainedAll }
    }
    try {
      let record = makeRecord()
      while (bytes(JSON.stringify([record])) > maxBytes && events.length > 1) {
        events = events.slice(Math.max(1, Math.floor(events.length / 4)))
        retainedAll = false; record = makeRecord()
      }
      if (bytes(JSON.stringify([record])) > maxBytes) return false
      const records = all().filter(saved => scopeKey(saved) !== scopeKey(scope)).sort((a, b) => a.savedAt - b.savedAt)
      records.push(record)
      while (records.length > maxScopes || bytes(JSON.stringify(records)) > maxBytes) records.shift()
      const encoded = JSON.stringify(records)
      storage.setItem(storageKey, encoded)
      return storage.getItem(storageKey) === encoded
    } catch { return false }
  }
  return {
    read, write,
    clear(scope: Pick<WorkspaceHistoryCacheScope, 'networkId' | 'deviceId'>) {
      const records = all().filter(record => record.networkId !== scope.networkId || record.deviceId !== scope.deviceId)
      const encoded = JSON.stringify(records)
      storage.setItem(storageKey, encoded)
      if (storage.getItem(storageKey) !== encoded) throw new Error('本地历史缓存尚未清除，请重试。')
    },
    stats(scope: Pick<WorkspaceHistoryCacheScope, 'networkId' | 'deviceId'>) {
      const records = all().filter(record => record.networkId === scope.networkId && record.deviceId === scope.deviceId)
      return { conversations: records.length, bytes: bytes(JSON.stringify(records)), maxBytes }
    },
    all(scope: Pick<WorkspaceHistoryCacheScope, 'networkId' | 'deviceId'>): CachedWorkspaceHistory[] {
      if (!scope.networkId || !scope.deviceId) return []
      return all().filter(record => record.networkId === scope.networkId && record.deviceId === scope.deviceId).map(display)
    },
    search(scope: WorkspaceHistoryCacheScope, query: string): CachedWorkspaceSearchItem[] {
      const needle = query.trim().toLocaleLowerCase()
      if (!needle) return []
      return read(scope)?.events.flatMap(event => {
        const body = event.body
        const message = body.type === 'input_posted' || body.type === 'message_posted' ? body : body.type === 'workspace_commanded' && body.command.kind === 'post_channel_message' ? body.command : null
        if (!message || !message.text.toLocaleLowerCase().includes(needle)) return []
        return [{ event_hash: event.event_hash, message_id: message.message_id, conversation_id: event.scope_id, text: message.text, created_at_ms: event.created_at_ms, speaker: event.speaker }]
      }) ?? []
    },
  }
}
