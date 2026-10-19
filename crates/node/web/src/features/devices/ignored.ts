import { useSyncExternalStore } from "react"

// "忽略" for join requests: hidden on this browser only, the request still expires on its own.
// ponytail: interim until unified design adds approve(reject=true).
const key = "vibeface.ignored-pairs"
const listeners = new Set<() => void>()
let cached: string[] | null = null

function read(): string[] {
  if (cached) return cached
  try { cached = JSON.parse(localStorage.getItem(key) ?? "[]") as string[] } catch { cached = [] }
  return cached
}

export function ignorePair(requestId: string) {
  cached = [...read().filter((id) => id !== requestId), requestId].slice(-100)
  try { localStorage.setItem(key, JSON.stringify(cached)) } catch { /* private mode keeps it in memory */ }
  listeners.forEach((listener) => listener())
}

export function useIgnoredPairs(): string[] {
  return useSyncExternalStore((listener) => { listeners.add(listener); return () => listeners.delete(listener) }, read, () => [])
}
