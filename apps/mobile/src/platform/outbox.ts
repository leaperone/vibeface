import type { SecureApi } from '@vibeface/client/secure-api.ts'
import { OutboxCorruptError } from '@vibeface/client/lib/outbox.ts'
import { validatedOutbox, OutboxChangedError, type OutboxLifecycle, type RawOutboxEntry } from '@vibeface/client/workspace/outbox-store.ts'

type MMKVOutbox = { getAllKeys(): string[]; getString(key: string): string | undefined; set(key: string, value: string): void; remove(key: string): void }
/** Production MMKV boundary. Keep raw strings untouched even if the last item is damaged. */
export function mobileOutbox(store: MMKVOutbox, secure: Pick<SecureApi, 'verify' | 'attestationPayload'>, lifecycle?: OutboxLifecycle) {
  const prefix = 'outbox:'
  const readRaw = () => store.getAllKeys().filter(key => key.startsWith(prefix)).map(key => {
    const raw = store.getString(key)
    if (typeof raw !== 'string') throw new OutboxCorruptError()
    return { key: key.slice(prefix.length), raw }
  })
  const snapshots = new WeakMap<RawOutboxEntry[], { key: string; raw: string }[]>()
  return validatedOutbox({
    async read() {
      const raw = readRaw()
      const entries = raw.map(entry => ({ key: entry.key, value: JSON.parse(entry.raw) as unknown }))
      snapshots.set(entries, raw)
      return entries
    },
    async commit(entries, mutation, guard) {
      guard()
      const before = snapshots.get(entries), now = readRaw()
      if (!before || before.length !== now.length || before.some(entry => !now.some(other => other.key === entry.key && other.raw === entry.raw))) throw new OutboxChangedError()
      guard()
      if ('put' in mutation) store.set(prefix + mutation.put.key, JSON.stringify(mutation.put))
      else store.remove(prefix + mutation.delete)
      snapshots.delete(entries)
    },
  }, secure, lifecycle)
}
