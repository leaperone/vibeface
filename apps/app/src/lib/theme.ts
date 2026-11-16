export type Theme = "system" | "light" | "dark"
const key = "vibeface.theme"

// Colors flip in one frame; otherwise every button and border would fade between the two palettes.
const still = "*,*::before,*::after{transition:none!important}"

/** Runs before first paint; keep it self-contained. */
export const themeScript = `(()=>{const t=()=>localStorage.getItem("${key}")||"system",m=matchMedia("(prefers-color-scheme: dark)"),r=document.documentElement;const a=()=>{const d=t()==="dark"||t()==="system"&&m.matches;if(r.dataset.theme===(d?"dark":"light"))return;const s=document.createElement("style");s.textContent="${still}";document.head.appendChild(s);r.dataset.theme=d?"dark":"light";r.dataset.accent="neutral";getComputedStyle(r).color;requestAnimationFrame(()=>s.remove())};a();m.addEventListener("change",()=>{if(t()==="system")a()})})()`

export function readTheme(): Theme {
  const value = typeof localStorage === "undefined" ? null : localStorage.getItem(key)
  return value === "light" || value === "dark" ? value : "system"
}

export function setTheme(theme: Theme) {
  localStorage.setItem(key, theme)
  const dark = theme === "dark" || theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches
  const root = document.documentElement
  if (root.dataset.theme === (dark ? "dark" : "light")) return
  const style = document.createElement("style")
  style.textContent = still
  document.head.appendChild(style)
  root.dataset.theme = dark ? "dark" : "light"
  root.dataset.accent = "neutral"
  getComputedStyle(root).color
  requestAnimationFrame(() => style.remove())
}
