export function readStorage<T>(key: string, fallback: T): T {
  if (typeof window === "undefined") return fallback
  try {
    return JSON.parse(window.localStorage.getItem(`vibeface.${key}`) ?? "null") ?? fallback
  } catch {
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

export function networkKey(key: string, networkId: string | undefined) {
  return `${key}.${networkId || "anonymous"}`
}

export function newId() {
  return globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}-${Math.random().toString(36).slice(2)}`
}

export function ensureClientId() {
  const existing = readStorage<string | null>("client", null)
  if (existing) return existing
  const created = newId()
  writeStorage("client", created)
  return created
}
