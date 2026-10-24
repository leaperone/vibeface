import type { OutboxItem, OutboxStore } from '@vibeface/client/lib/outbox.ts'

/** The outbox in this origin's IndexedDB, next to the device key. */
export function indexedOutbox(name = 'vibeface-outbox'): OutboxStore {
  const run = <T,>(mode: IDBTransactionMode, operation: (store: IDBObjectStore) => IDBRequest<T>) => new Promise<T>((resolve, reject) => {
    const open = indexedDB.open(name, 1)
    open.onupgradeneeded = () => open.result.createObjectStore('writes', { keyPath: 'key' })
    open.onerror = () => reject(new Error('无法打开浏览器待确认记录。'))
    open.onsuccess = () => {
      const db = open.result
      const transaction = db.transaction('writes', mode)
      const request = operation(transaction.objectStore('writes'))
      transaction.oncomplete = () => { db.close(); resolve(request.result) }
      transaction.onerror = transaction.onabort = () => { db.close(); reject(new Error('无法保存浏览器待确认记录。')) }
    }
  })
  return {
    all: () => run('readonly', store => store.getAll() as IDBRequest<OutboxItem[]>),
    put: async item => { await run('readwrite', store => store.put(item)) },
    delete: async key => { await run('readwrite', store => store.delete(key)) },
  }
}
