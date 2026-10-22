/**
 * Signed writes the entry has not confirmed. A write is kept byte-for-byte until the network
 * shows it recorded; it is never re-signed or sent to a different conversation.
 */
export type OutboxItem = {
  key: string
  method: string
  params: Record<string, unknown>
  created_at_ms: number
  rejected?: string
}

export interface OutboxStore {
  all(): Promise<OutboxItem[]>
  put(item: OutboxItem): Promise<void>
  delete(key: string): Promise<void>
}

const same = (left: unknown, right: unknown) => JSON.stringify(left) === JSON.stringify(right)

/** Keeps the first request for a business ID; a different request under the same ID is refused. */
export async function enqueue(store: OutboxStore, item: OutboxItem): Promise<OutboxItem> {
  const existing = (await store.all()).find(entry => entry.key === item.key)
  if (existing) {
    if (existing.method !== item.method || !same(existing.params, item.params)) throw new Error('这项操作还有一份未确认的不同内容。请先确认原操作。')
    return existing
  }
  await store.put(item)
  return item
}

export type Settled = { confirmed: string[]; resubmitted: string[]; rejected: string[]; unknown: string | null }

/**
 * Checks each pending write by its business ID, then resubmits the unchanged request.
 * Stops at the first unknown outcome, so nothing runs twice and order is kept.
 */
export async function settle(store: OutboxStore, network: {
  recorded(item: OutboxItem): Promise<boolean>
  submit(item: OutboxItem): Promise<unknown>
}): Promise<Settled> {
  const result: Settled = { confirmed: [], resubmitted: [], rejected: [], unknown: null }
  const pending = (await store.all()).filter(item => item.rejected === undefined).sort((a, b) => a.created_at_ms - b.created_at_ms)
  for (const item of pending) {
    try {
      if (await network.recorded(item)) { await store.delete(item.key); result.confirmed.push(item.key); continue }
      await network.submit(item)
      await store.delete(item.key)
      result.resubmitted.push(item.key)
    } catch (caught) {
      if (caught instanceof Error && 'responded' in caught && caught.responded === true) {
        await store.put({ ...item, rejected: caught.message })
        result.rejected.push(item.key)
        continue
      }
      result.unknown = item.key
      break
    }
  }
  return result
}
