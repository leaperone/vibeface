import { useSyncExternalStore } from "react"

// The onboarding step (3 or 4) after a new network is created here; cleared once the bot step is done or skipped.
const key = "vibeface.onboarding"
const listeners = new Set<() => void>()
type Step = 3 | 4
const read = (): Step | null => { try { const value = localStorage.getItem(key); return value === "3" || value === "4" ? Number(value) as Step : null } catch { return null } }
function write(step: Step | null) {
  try { if (step) localStorage.setItem(key, String(step)); else localStorage.removeItem(key) } catch {}
  listeners.forEach((listener) => listener())
}
export const startOnboarding = () => write(3)
export const setOnboardingStep = (step: Step) => write(step)
export const finishOnboarding = () => write(null)
export const useOnboarding = () => useSyncExternalStore((listener) => { listeners.add(listener); return () => listeners.delete(listener) }, read, () => null)
