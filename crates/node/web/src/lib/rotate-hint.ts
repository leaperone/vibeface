import { useSyncExternalStore } from 'react'

/** After a manager stopped a recovery, its file may be out there: a reminder to change the password, kept until done or dismissed. */
const key = (network: string) => `vibeface.rotate-hint.${network}`
const listeners = new Set<() => void>()
const subscribe = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } }

export function setRotateHint(network: string, on: boolean) {
  try { if (on) localStorage.setItem(key(network), '1'); else localStorage.removeItem(key(network)) } catch {}
  for (const listener of listeners) listener()
}

export function useRotateHint(network: string | undefined) {
  return useSyncExternalStore(subscribe, () => { try { return !!network && localStorage.getItem(key(network)) === '1' } catch { return false } }, () => false)
}
