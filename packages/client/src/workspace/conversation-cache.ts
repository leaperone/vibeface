import type { KeyValue } from '../lib/platform.ts'
import { parseConversation, record } from './model.ts'
import type { ConversationPayload } from './types.ts'

type ConversationCacheScope = { networkId: string; deviceId: string }
type ConversationCacheKey = ConversationCacheScope & { conversationId: string }
export type CachedConversation = ConversationCacheKey & { savedAt: number; payload: ConversationPayload }
/** The index of `[slot, revision]` pairs, oldest first; each conversation is stored under this key, a dot and its slot. */
export const conversationCacheStorageKey = 'vibeface.conversation-cache'
export const conversationSlotKey = (slot: number) => `${conversationCacheStorageKey}.${slot}`
const conversationCacheLimits = { maxBytes: 12 * 1024 * 1024, maxConversations: 40 }
type Stored = { record: CachedConversation; slot: number; revision: number; bytes: number }

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

function parseIndex(raw: string | null): [number, number][] {
  if (raw === null) return []
  try {
    const value: unknown = JSON.parse(raw)
    if (!Array.isArray(value)) return []
    return value.filter((item): item is [number, number] => Array.isArray(item) && item.length === 2 && Number.isSafeInteger(item[0]) && item[0] >= 0 && typeof item[1] === 'number')
  } catch { return [] }
}

const sameScope = (item: ConversationCacheScope, scope: ConversationCacheScope) => item.networkId === scope.networkId && item.deviceId === scope.deviceId
const sameKey = (item: ConversationCacheKey, key: ConversationCacheKey) => sameScope(item, key) && item.conversationId === key.conversationId
const bytes = (value: string) => new TextEncoder().encode(value).length
// A slot never changes content under one revision, so cache instances over the same storage share what they parsed.
const parsedSlots = new WeakMap<KeyValue, Map<number, Stored>>()

/** Display-only snapshots. No connection, write permission, outbox settlement or notification receipt is stored. */
export function createConversationCache(options: { storage: KeyValue; maxBytes?: number; maxConversations?: number; now?: () => number }) {
  const maxBytes = options.maxBytes ?? conversationCacheLimits.maxBytes
  const maxConversations = options.maxConversations ?? conversationCacheLimits.maxConversations
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 2 || !Number.isSafeInteger(maxConversations) || maxConversations < 1) throw new Error('本地会话缓存的容量不对。')
  const parsed = parsedSlots.get(options.storage) ?? new Map<number, Stored>()
  parsedSlots.set(options.storage, parsed)
  let previousRaw: string | null | undefined
  let entries: Stored[] = []
  // Every slot the stored index names, even ones dropped on load, so a write never overwrites what the index still points to.
  let indexed: number[] = []
  const bound = (items: Stored[]) => {
    const kept = items.slice(-maxConversations)
    let total = kept.reduce((sum, item) => sum + item.bytes, 0)
    // Evict whole conversations: trimming arrays would falsely preserve their complete history coverage.
    while (total > maxBytes) total -= kept.shift()?.bytes ?? total
    return kept
  }
  const readSlot = (slot: number, revision: number): Stored | null => {
    try {
      const value = options.storage.getItem(conversationSlotKey(slot)) ?? ''
      const stored = value ? record(JSON.parse(value)) : null
      const entry = stored?.revision === revision ? parseEntry(stored) : null
      return entry && { record: entry, slot, revision, bytes: bytes(value) }
    } catch { return null }
  }
  const load = () => {
    let raw: string | null
    try { raw = options.storage.getItem(conversationCacheStorageKey) }
    catch { return entries }
    if (raw === previousRaw) return entries
    previousRaw = raw
    const index = parseIndex(raw)
    indexed = index.map(([slot]) => slot)
    for (const slot of parsed.keys()) if (!indexed.includes(slot)) parsed.delete(slot)
    let next: Stored[] = []
    for (const [slot, revision] of index) {
      const same = parsed.get(slot), item = same?.revision === revision ? same : readSlot(slot, revision)
      if (item) parsed.set(slot, item)
      if (item && item.bytes <= maxBytes) next = [...next.filter((entry) => !sameKey(entry.record, item.record)), item]
    }
    entries = bound(next.sort((left, right) => left.record.savedAt - right.record.savedAt))
    return entries
  }
  const persist = (items: Stored[], written?: { item: Stored; raw: string }) => {
    const next = bound(items), raw = JSON.stringify(next.map((item) => [item.slot, item.revision]))
    // The active session can still switch between memory snapshots if disk storage becomes unavailable.
    entries = next
    if (written) {
      options.storage.setItem(conversationSlotKey(written.item.slot), written.raw)
      parsed.set(written.item.slot, written.item)
    }
    options.storage.setItem(conversationCacheStorageKey, raw)
    previousRaw = raw
    const kept = new Set(next.map((item) => item.slot))
    // Evicted, replaced and cleared conversations leave no readable copy behind.
    for (const slot of new Set([...indexed, ...items.map((item) => item.slot)])) {
      if (kept.has(slot)) continue
      options.storage.setItem(conversationSlotKey(slot), '')
      parsed.delete(slot)
    }
    indexed = [...kept]
  }
  const copy = (item: CachedConversation): CachedConversation => ({ ...item, payload: parsePayload(item.payload, item.conversationId) })
  return {
    read(key: ConversationCacheKey): CachedConversation | null {
      const item = load().find((entry) => sameKey(entry.record, key))
      return item ? copy(item.record) : null
    },
    /** Shared snapshots: each stays the same object until its conversation is saved again, so callers must not modify them. */
    all(scope: ConversationCacheScope): CachedConversation[] { return load().filter((item) => sameScope(item.record, scope)).map((item) => item.record) },
    save(input: ConversationCacheKey & { payload: ConversationPayload }): void {
      const entry = parseEntry({ ...input, savedAt: (options.now ?? Date.now)() })
      if (!entry) throw new Error('不能缓存这段会话，记录格式不完整。')
      const previous = load(), revision = Math.random(), raw = JSON.stringify({ ...entry, revision }), size = bytes(raw)
      // An oversized conversation must not evict every other useful snapshot.
      if (size > maxBytes) return
      // A new slot keeps the indexed copy readable until the index names this one.
      const used = new Set([...indexed, ...previous.map((item) => item.slot)])
      let slot = 0
      while (used.has(slot)) slot += 1
      const item = { record: entry, slot, revision, bytes: size }
      persist([...previous.filter((saved) => !sameKey(saved.record, input)), item], { item, raw })
    },
    clear(scope: ConversationCacheScope & { conversationId?: string }): void {
      persist(load().filter((item) => !sameScope(item.record, scope) || scope.conversationId !== undefined && item.record.conversationId !== scope.conversationId))
    },
    stats(scope: ConversationCacheScope) {
      const items = load().filter((item) => sameScope(item.record, scope))
      return { conversations: items.length, bytes: items.reduce((sum, item) => sum + item.bytes, 0), maxBytes, maxConversations }
    },
  }
}
