export function readStorage<T>(key: string, fallback: T, required = false): T {
  if (typeof window === "undefined") {
    if (required) throw new Error("浏览器无法读取待核对记录。")
    return fallback
  }
  try {
    return JSON.parse(window.localStorage.getItem(`vibeface.${key}`) ?? "null") ?? fallback
  } catch {
    if (required) throw new Error("无法读取本地待核对记录，已暂停写入。请保留浏览器数据并核查。")
    return fallback
  }
}

export function writeStorage(key: string, value: unknown, required = false) {
  if (typeof window === "undefined") {
    if (required) throw new Error("浏览器无法保存操作编号。请允许此站点使用本地存储后重试。")
    return
  }
  try {
    window.localStorage.setItem(`vibeface.${key}`, JSON.stringify(value))
  } catch {
    if (required) throw new Error("浏览器无法保存操作编号。请允许此站点使用本地存储后重试。")
  }
}

export function newId() {
  return globalThis.crypto.randomUUID()
}
