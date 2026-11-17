import type { KeyValue } from '../lib/platform.ts'
import { parseConversation, record } from './model.ts'
import type { ConversationPayload } from './types.ts'

export type ConversationCacheScope = { networkId: string; deviceId: string }
export type ConversationCacheKey = ConversationCacheScope & { conversationId: string }
export type CachedConversation = ConversationCacheKey & { savedAt: number; payload: ConversationPayload }
export const conversationCacheStorageKey = 'vibeface.conversation-cache'
export const conversationCacheLimits = { maxBytes: 12 * 1024 * 1024, maxConversations: 40 }

/** The parser consumes local jobs, whereas the presentation payload also contains merged owner jobs. */
function parsePayload(value: unknown, conversationId: string): ConversationPayload {
  const payload = record(value)
  const scheduler = payload.scheduler == null ? null : record(payload.scheduler)
  return parseConversation({ ...payload, scheduler: scheduler && { ...scheduler, jobs: scheduler.local_jobs } }, conversationId)
}

function parseEntry(value: unknown): CachedConversation | null {
  try {
    const item = record(value)
    if (typeof item.networkId !== 'string' || !item.networkId || typeof item.deviceId !== 'string' || !item.deviceId || typeof item.conversationId !== 'string' || !item.conversationId
      || typeof item.savedAt !== 'number' || !Number.isSafeInteger(item.savedAt) || item.savedAt < 0) return null
    return { networkId: item.networkId, deviceId: item.deviceId, conversationId: item.conversationId, savedAt: item.savedAt, payload: parsePayload(item.payload, item.conversationId) }
  } catch { return null }
}

const sameScope = (item: ConversationCacheScope, scope: ConversationCacheScope) => item.networkId === scope.networkId && item.deviceId === scope.deviceId
const sameKey = (item: ConversationCacheKey, key: ConversationCacheKey) => sameScope(item, key) && item.conversationId === key.conversationId
const bytes = (value: string) => new TextEncoder().encode(value).length

/** Display-only snapshots. No connection, write permission, outbox settlement or notification receipt is stored. */
export function createConversationCache(options: { storage: KeyValue; maxBytes?: number; maxConversations?: number; now?: () => number }) {
  const maxBytes = options.maxBytes ?? conversationCacheLimits.maxBytes
  const maxConversations = options.maxConversations ?? conversationCacheLimits.maxConversations
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 2 || !Number.isSafeInteger(maxConversations) || maxConversations < 1) throw new Error('本地会话缓存的容量不对。')
  let previousRaw: string | null | undefined
  let entries: CachedConversation[] = []
  const bound = (items: CachedConversation[]) => {
    const kept = items.slice(-maxConversations)
    // Evict whole conversations: trimming arrays would falsely preserve their complete history coverage.
    while (kept.length && bytes(JSON.stringify(kept)) > maxBytes) kept.shift()
    return kept
  }
  const load = () => {
    let raw: string | null
    try { raw = options.storage.getItem(conversationCacheStorageKey) }
    catch { return entries }
    if (raw === previousRaw) return entries
    previousRaw = raw
    entries = []
    if (raw === null || bytes(raw) > maxBytes) return entries
    try {
      const value: unknown = JSON.parse(raw)
      if (!Array.isArray(value)) return entries
      for (const entry of value) {
        const parsed = parseEntry(entry)
        if (parsed) entries = [...entries.filter((item) => !sameKey(item, parsed)), parsed]
      }
      entries = bound(entries.sort((left, right) => left.savedAt - right.savedAt))
    } catch { entries = [] }
    return entries
  }
  const persist = (items: CachedConversation[]) => {
    const next = bound(items), raw = JSON.stringify(next)
    // The active session can still switch between memory snapshots if disk storage becomes unavailable.
    entries = next
    options.storage.setItem(conversationCacheStorageKey, raw)
    previousRaw = raw
  }
  const copy = (item: CachedConversation): CachedConversation => ({ ...item, payload: parsePayload(item.payload, item.conversationId) })
  return {
    read(key: ConversationCacheKey): CachedConversation | null {
      const item = load().find((entry) => sameKey(entry, key))
      return item ? copy(item) : null
    },
    all(scope: ConversationCacheScope): CachedConversation[] { return load().filter((item) => sameScope(item, scope)).map(copy) },
    save(input: ConversationCacheKey & { payload: ConversationPayload }): void {
      const entry = parseEntry({ ...input, savedAt: (options.now ?? Date.now)() })
      if (!entry) throw new Error('不能缓存这段会话，记录格式不完整。')
      const previous = load()
      // An oversized conversation must not evict every other useful snapshot.
      if (bytes(JSON.stringify([entry])) > maxBytes) return
      persist([...previous.filter((item) => !sameKey(item, input)), entry])
    },
    clear(scope: ConversationCacheScope & { conversationId?: string }): void {
      persist(load().filter((item) => !sameScope(item, scope) || scope.conversationId !== undefined && item.conversationId !== scope.conversationId))
    },
    stats(scope: ConversationCacheScope) {
      const items = load().filter((item) => sameScope(item, scope))
      return { conversations: items.length, bytes: bytes(JSON.stringify(items)), maxBytes, maxConversations }
    },
  }
}

export type ConversationCache = ReturnType<typeof createConversationCache>
