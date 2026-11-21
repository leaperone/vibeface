import { useSyncExternalStore } from "react"

// The displayed onboarding step (4 or 5) after a new network is created here, saved as `<network_id>:<step>` so only that network's
// connection shows it; cleared once the bot step is done or skipped, or when creating the network fails.
const key = "vibeface.onboarding"
const listeners = new Set<() => void>()
type Step = 4 | 5
const read = () => { try { return localStorage.getItem(key) } catch { return null } }
function write(value: string | null) {
  try { if (value) localStorage.setItem(key, value); else localStorage.removeItem(key) } catch {}
  listeners.forEach((listener) => listener())
}
export const startOnboarding = (networkId: string) => write(`${networkId}:4`)
export const setOnboardingStep = (networkId: string, step: Step) => write(`${networkId}:${step}`)
export const finishOnboarding = () => write(null)
export function useOnboarding(networkId: string | undefined): Step | null {
  const value = useSyncExternalStore((listener) => { listeners.add(listener); return () => listeners.delete(listener) }, read, () => null)
  return networkId && (value === `${networkId}:4` || value === `${networkId}:5`) ? Number(value.slice(-1)) as Step : null
}
