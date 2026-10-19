export function readStorage<T>(key: string, fallback: T, required = false): T {
  if (typeof window === "undefined") {
    if (required) throw new Error("浏览器读不了需要确认的记录。")
    return fallback
  }
  try {
    return JSON.parse(window.localStorage.getItem(`vibeface.${key}`) ?? "null") ?? fallback
  } catch {
    if (required) throw new Error("读不了本地需要确认的记录，已暂停发送。请不要清除浏览器数据。")
    return fallback
  }
}

export function writeStorage(key: string, value: unknown, required = false) {
  if (typeof window === "undefined") {
    if (required) throw new Error("浏览器保存不了这条记录。请允许此站点使用本地存储后重试。")
    return
  }
  try {
    window.localStorage.setItem(`vibeface.${key}`, JSON.stringify(value))
  } catch {
    if (required) throw new Error("浏览器保存不了这条记录。请允许此站点使用本地存储后重试。")
  }
}

export function networkKey(key: string, networkId: string) {
  return `${key}.${networkId}`
}

export function newId() {
  return globalThis.crypto.randomUUID()
}
