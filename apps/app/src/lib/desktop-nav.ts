import { getRouter } from '@/router'

declare global {
  interface Window {
    __vibefaceNavigate?: (path: string) => void
  }
}

/** Lets the desktop host open in-window routes without a full reload. */
export function installDesktopNavigation(): void {
  if (typeof window === 'undefined' || !window.vibefaceClient) return
  window.__vibefaceNavigate = (path: string) => {
    void getRouter().navigate({ to: path })
  }
}
