import { useSyncExternalStore } from "react"

const followDark = (change: () => void) => {
  const observer = new MutationObserver(change)
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] })
  return () => observer.disconnect()
}

export const useDark = () => useSyncExternalStore(followDark, () => document.documentElement.dataset.theme === "dark", () => false)
