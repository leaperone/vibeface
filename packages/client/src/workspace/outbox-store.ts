import { OutboxCorruptError, type OutboxItem, type OutboxStore } from '../lib/outbox.ts'
import type { SecureApi } from '../secure-api.ts'
import { decodeOutbox, verifyOutbox } from './outbox-codec.ts'

export class OutboxChangedError extends Error {}
export type RawOutboxEntry = { key: unknown; value: unknown }
export type OutboxLifecycle = {
  run<T>(operation: (current: () => boolean) => Promise<T>): Promise<T>
  lease(): () => boolean
}
/** Latch shared by all workspaces using this physical ledger. Reading never repairs storage. */
export function validatedOutbox(raw: {
  read(): Promise<RawOutboxEntry[]>
  commit(snapshot: RawOutboxEntry[], mutation: { put: OutboxItem } | { delete: string }, guard: () => void): Promise<void>
}, secure: Pick<SecureApi, 'verify' | 'attestationPayload'>, lifecycle?: OutboxLifecycle): OutboxStore & { reset(): void } {
  let corrupt = false
  const listeners = new Set<() => void>()
  const guard = () => { if (corrupt) throw new OutboxCorruptError() }
  const stop = (): never => { corrupt = true; for (const listener of listeners) listener(); throw new OutboxCorruptError() }
  const lease = () => {
    const current = lifecycle?.lease() ?? (() => true)
    return () => { if (!current()) throw new Error('应用内数据已切换，请重新打开工作区。'); guard() }
  }
  const run = <T,>(operation: (guard: () => void) => Promise<T>): Promise<T> => {
    const check = lease()
    const work = async (current: () => boolean) => {
      const valid = () => { if (!current()) throw new Error('应用内数据已切换，请重新打开工作区。'); check() }
      valid()
      try { return await operation(valid) }
      catch (cause) { valid(); if (cause instanceof OutboxCorruptError) stop(); throw cause }
    }
    return lifecycle ? lifecycle.run(work) : work(() => true)
  }
  const batch = async (check: () => void) => {
    try {
      const entries = await raw.read(); check()
      const items = entries.map(entry => decodeOutbox(entry.value, entry.key))
      if (new Set(items.map(item => item.key)).size !== items.length) throw new OutboxCorruptError()
      await verifyOutbox(items, secure); check()
      return { entries, items }
    } catch { check(); return stop() }
  }
  return {
    guard, lease, corrupt: stop,
    watch(listener) { listeners.add(listener); return () => listeners.delete(listener) },
    reset() { corrupt = false; for (const listener of listeners) listener() },
    all: () => run(async check => (await batch(check)).items),
    put: item => run(async check => {
      for (;;) {
        const { entries, items } = await batch(check)
        let validated: OutboxItem
        try {
          validated = decodeOutbox(item, item.key); await verifyOutbox([validated], secure)
          const previous = items.find(entry => entry.key === item.key)
          if (previous && (previous.created_at_ms !== item.created_at_ms || previous.method !== item.method || previous.scope_id !== item.scope_id || JSON.stringify(previous.params) !== JSON.stringify(item.params))) throw new OutboxCorruptError()
        } catch { check(); return stop() }
        check()
        try { await raw.commit(entries, { put: validated }, check); check(); return }
        catch (cause) { if (!(cause instanceof OutboxChangedError)) throw cause }
      }
    }),
    delete: key => run(async check => {
      for (;;) {
        const { entries } = await batch(check)
        check()
        try { await raw.commit(entries, { delete: key }, check); check(); return }
        catch (cause) { if (!(cause instanceof OutboxChangedError)) throw cause }
      }
    }),
  }
}
