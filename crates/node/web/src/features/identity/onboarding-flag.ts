import { useSyncExternalStore } from "react"

// Set when a new network is created here; cleared once the assistant step is done or skipped.
const key = "vibeface.onboarding"
const listeners = new Set<() => void>()
const read = () => { try { return localStorage.getItem(key) === "1" } catch { return false } }
function write(on: boolean) {
  try { if (on) localStorage.setItem(key, "1"); else localStorage.removeItem(key) } catch {}
  listeners.forEach((listener) => listener())
}
export const startOnboarding = () => write(true)
export const finishOnboarding = () => write(false)
export const useOnboarding = () => useSyncExternalStore((listener) => { listeners.add(listener); return () => listeners.delete(listener) }, read, () => false)
