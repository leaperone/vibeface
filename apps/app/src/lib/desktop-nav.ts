import type { AnyRouter } from '@tanstack/react-router'

declare global {
  interface Window {
    __vibefaceNavigate?: (path: string) => void
  }
}

/** Lets the desktop host open in-window routes without a full reload. */
export function installDesktopNavigation(router: AnyRouter): void {
  if (typeof window === 'undefined' || !window.vibefaceClient) return
  window.__vibefaceNavigate = (path: string) => {
    void router.navigate({ to: path })
  }
}
