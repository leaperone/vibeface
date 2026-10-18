import { useEffect } from "react"

export function ViewportHeight() {
  useEffect(() => {
    const viewport = window.visualViewport
    const sync = () => {
      const height = viewport?.height ?? window.innerHeight
      document.documentElement.style.setProperty("--app-height", `${Math.round(height)}px`)
    }
    sync()
    viewport?.addEventListener("resize", sync)
    window.addEventListener("orientationchange", sync)
    return () => {
      viewport?.removeEventListener("resize", sync)
      window.removeEventListener("orientationchange", sync)
    }
  }, [])
  return null
}
