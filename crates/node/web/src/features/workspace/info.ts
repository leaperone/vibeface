import { parseInfo } from "./model"
import type { EntryInfo } from "./types"

export async function loadInfo(): Promise<EntryInfo> {
  let response: Response
  try {
    response = await fetch("/api/info", { credentials: "same-origin" })
  } catch {
    throw new Error("连接中断，尚未确认操作结果。请刷新或核对操作记录，不要重复派发。")
  }
  const data: unknown = await response.json().catch(() => ({}))
  if (!response.ok) {
    const body = data && typeof data === "object" ? (data as { error?: unknown; message?: unknown }) : {}
    const nested = body.error && typeof body.error === "object" ? (body.error as { message?: unknown }).message : undefined
    const message = typeof body.error === "string"
      ? body.error
      : typeof nested === "string"
        ? nested
        : typeof body.message === "string"
          ? body.message
          : `请求失败（${response.status}）`
    throw new Error(message)
  }
  return parseInfo(data)
}
