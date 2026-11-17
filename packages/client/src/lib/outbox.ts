import type { SignedRequest, WriteMethod } from '../workspace/write-request.ts'
import type { InputReceipt, WorkspaceCommandNotSaved } from '../workspace/types.ts'

/**
 * Signed writes the entry has not confirmed. A write is kept byte-for-byte until the network
 * shows it recorded; it is never re-signed or sent to a different conversation.
 */
type OutboxMetadata = {
  key: string
  scope_id: string
  created_at_ms: number
  rejected?: string
  rejection_code?: 'input_not_saved' | 'input_conflict' | 'input_missing'
  /** Local message bytes stay here until the Node has the signed input. */
  uploads?: { hash: string; size: number; name: string; data: string }[]
  delivery?: 'saved' | 'uploading' | 'submitting' | 'accepted' | 'unknown'
  /** Bound before the first message call; an uncertain request never changes entry. */
  entry_device_id?: string
  receipt?: InputReceipt
  release_expected_event?: string
  release_command_id?: string
  command_rejection?: WorkspaceCommandNotSaved
  prerequisite?: OutboxRecord<'workspace_command'>
}

export type OutboxRecord<M extends WriteMethod> = OutboxMetadata & { method: M; params: SignedRequest<M> } & (M extends 'message' ? { delivery: NonNullable<OutboxMetadata['delivery']> } : {})
export type OutboxItem = { [M in WriteMethod]: OutboxRecord<M> }[WriteMethod]

export const outboxCorruptNotice = '本地待确认记录损坏，已暂停发送。请保留应用数据以恢复原记录。'
export class OutboxCorruptError extends Error {
  constructor() { super(outboxCorruptNotice); this.name = 'OutboxCorruptError' }
}
export interface OutboxStore {
  /** A synchronous latch checked again after every asynchronous boundary. */
  lease?(): () => void
  guard?(): void
  corrupt?(): never
  watch?(listener: () => void): () => void
  all(): Promise<OutboxItem[]>
  put(item: OutboxItem): Promise<void>
  delete(key: string): Promise<void>
}

const same = (left: unknown, right: unknown) => JSON.stringify(left) === JSON.stringify(right)

/** Keeps the first request for a business ID; a different request under the same ID is refused. */
export async function enqueue(store: OutboxStore, item: OutboxItem): Promise<OutboxItem> {
  const check = store.lease?.() ?? (() => store.guard?.())
  check()
  const existing = (await store.all()).find(entry => entry.key === item.key)
  check()
  if (existing) {
    if (existing.scope_id !== item.scope_id || existing.method !== item.method || !same(existing.params, item.params)) throw new Error('这项操作还有一份未确认的不同内容。先确认原操作。')
    return existing
  }
  await store.put(item)
  check()
  return item
}

type Settled = { confirmed: string[]; resubmitted: string[]; rejected: string[]; unknown: string | null }

/**
 * Checks each pending write by its business ID, then resubmits the unchanged request.
 * Stops at the first unknown outcome, so nothing runs twice and order is kept.
 */
export async function settle(store: OutboxStore, network: {
  recorded(item: OutboxItem): Promise<boolean>
  submit(item: OutboxItem): Promise<unknown>
}): Promise<Settled> {
  const check = store.lease?.() ?? (() => store.guard?.())
  check()
  const result: Settled = { confirmed: [], resubmitted: [], rejected: [], unknown: null }
  const pending = (await store.all()).filter(item => item.rejected === undefined).sort((a, b) => a.created_at_ms - b.created_at_ms)
  for (const item of pending) {
    try {
      check()
      if (await network.recorded(item)) { check(); await store.delete(item.key); check(); result.confirmed.push(item.key); continue }
      check()
      await network.submit(item)
      check()
      await store.delete(item.key)
      check()
      result.resubmitted.push(item.key)
    } catch (caught) {
      check()
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
