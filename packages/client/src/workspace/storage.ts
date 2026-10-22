import type { KeyValue } from '../lib/platform.ts'

/** JSON values under `vibeface.` keys; `required` turns a failed read or write into an error instead of the fallback. */
export function storageOf(storage: KeyValue) {
  function readStorage<T>(key: string, fallback: T, required = false): T {
    try {
      return JSON.parse(storage.getItem(`vibeface.${key}`) ?? "null") ?? fallback
    } catch {
      if (required) throw new Error("读不了本地需要确认的记录，已暂停发送。请不要清除这台设备上的应用数据。")
      return fallback
    }
  }
  function writeStorage(key: string, value: unknown, required = false) {
    try {
      storage.setItem(`vibeface.${key}`, JSON.stringify(value))
    } catch {
      if (required) throw new Error("保存不了这条记录。请允许保存本地数据后重试。")
    }
  }
  return { readStorage, writeStorage }
}

export function networkKey(key: string, networkId: string) {
  return `${key}.${networkId}`
}

export function newId() {
  return globalThis.crypto.randomUUID()
}
