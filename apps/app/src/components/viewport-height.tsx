import { useEffect } from "react"

export function ViewportHeight() {
  useEffect(() => {
    const viewport = window.visualViewport
    const sync = () => {
      const height = viewport?.height ?? window.innerHeight
      document.documentElement.style.setProperty("--app-height", `${Math.round(height)}px`)
      document.documentElement.style.setProperty("--app-top", `${Math.round(viewport?.offsetTop ?? 0)}px`)
    }
    sync()
    viewport?.addEventListener("resize", sync)
    viewport?.addEventListener("scroll", sync)
    window.addEventListener("orientationchange", sync)
    return () => {
      viewport?.removeEventListener("resize", sync)
      viewport?.removeEventListener("scroll", sync)
      window.removeEventListener("orientationchange", sync)
    }
  }, [])
  return null
}
