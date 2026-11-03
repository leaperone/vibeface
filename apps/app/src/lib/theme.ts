export type Theme = "system" | "light" | "dark"
const key = "vibeface.theme"

// Colors flip in one frame; otherwise every button and border would fade between the two palettes.
const still = "*,*::before,*::after{transition:none!important}"

/** Runs before first paint; keep it self-contained. */
export const themeScript = `(()=>{const t=()=>localStorage.getItem("${key}")||"system",m=matchMedia("(prefers-color-scheme: dark)"),r=document.documentElement;const a=()=>{const d=t()==="dark"||t()==="system"&&m.matches;if(r.classList.contains("dark")===d)return;const s=document.createElement("style");s.textContent="${still}";document.head.appendChild(s);r.classList.toggle("dark",d);getComputedStyle(r).color;requestAnimationFrame(()=>s.remove())};a();m.addEventListener("change",()=>{if(t()==="system")a()})})()`

export function readTheme(): Theme {
  const value = typeof localStorage === "undefined" ? null : localStorage.getItem(key)
  return value === "light" || value === "dark" ? value : "system"
}

export function setTheme(theme: Theme) {
  localStorage.setItem(key, theme)
  const dark = theme === "dark" || theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches
  const root = document.documentElement
  if (root.classList.contains("dark") === dark) return
  const style = document.createElement("style")
  style.textContent = still
  document.head.appendChild(style)
  root.classList.toggle("dark", dark)
  getComputedStyle(root).color
  requestAnimationFrame(() => style.remove())
}
