import type { SecureApi } from '@vibeface/client/secure-api.ts'
import { OutboxCorruptError } from '@vibeface/client/lib/outbox.ts'
import { validatedOutbox, OutboxChangedError, type OutboxLifecycle, type RawOutboxEntry } from '@vibeface/client/workspace/outbox-store.ts'

/** Compare structured clones without JSON's loss of undefined, NaN or array holes.
 * A validated snapshot contains only plain records/arrays; other object kinds differ.
 */
function sameClone(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true
  if (!left || !right || typeof left !== 'object' || typeof right !== 'object') return false
  if (Object.getPrototypeOf(left) !== Object.getPrototypeOf(right)) return false
  if (Array.isArray(left) !== Array.isArray(right)) return false
  const leftKeys = Reflect.ownKeys(left), rightKeys = Reflect.ownKeys(right)
  if (leftKeys.length !== rightKeys.length) return false
  return leftKeys.every(key => Object.prototype.hasOwnProperty.call(right, key)
    && sameClone(Reflect.get(left, key), Reflect.get(right, key)))
}

/** Both physical keys and structured-clone values are checked before any mutation. */
export function indexedOutbox(secure: Pick<SecureApi, 'verify' | 'attestationPayload'>, name = 'vibeface-outbox', lifecycle?: OutboxLifecycle) {
  const open = () => new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open(name, 1)
    request.onupgradeneeded = () => request.result.createObjectStore('writes', { keyPath: 'key' })
    request.onerror = () => reject(new OutboxCorruptError())
    request.onsuccess = () => resolve(request.result)
  })
  const transaction = async <T,>(mode: IDBTransactionMode, operation: (store: IDBObjectStore, done: (value: T) => void, abort: (cause?: unknown) => void) => void) => {
    const db = await open()
    try {
      return await new Promise<T>((resolve, reject) => {
        const tx = db.transaction('writes', mode)
        let result: T, failure: unknown
        tx.oncomplete = () => resolve(result)
        tx.onerror = tx.onabort = () => reject(failure ?? new OutboxCorruptError())
        try { operation(tx.objectStore('writes'), value => { result = value }, (cause) => { failure = cause; tx.abort() }) }
        catch { tx.abort() }
      })
    } finally { db.close() }
  }
  const entries = (store: IDBObjectStore, done: (entries: RawOutboxEntry[]) => void) => {
    const keys = store.getAllKeys(), values = store.getAll()
    values.onsuccess = () => done(values.result.map((value: unknown, index: number) => ({ key: keys.result[index], value })))
  }
  return validatedOutbox({
    read: () => transaction('readonly', (store, done) => entries(store, done)),
    commit: (snapshot, mutation, guard) => transaction<void>('readwrite', (store, done, abort) => entries(store, current => {
      try {
        guard()
        if (!sameClone(current, snapshot)) throw new OutboxChangedError()
        if ('put' in mutation) store.put(mutation.put)
        else store.delete(mutation.delete)
        done(undefined)
      } catch (cause) { abort(cause) }
    })),
  }, secure, lifecycle)
}
