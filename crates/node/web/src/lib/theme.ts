export type Theme = "system" | "light" | "dark"
const key = "vibeface.theme"

/** Runs before first paint; keep it self-contained. */
export const themeScript = `(()=>{const t=localStorage.getItem("${key}")||"system",m=matchMedia("(prefers-color-scheme: dark)");const a=()=>document.documentElement.classList.toggle("dark",t==="dark"||t==="system"&&m.matches);a();m.addEventListener("change",()=>{if((localStorage.getItem("${key}")||"system")==="system")a()})})()`

export function readTheme(): Theme {
  const value = typeof localStorage === "undefined" ? null : localStorage.getItem(key)
  return value === "light" || value === "dark" ? value : "system"
}

export function setTheme(theme: Theme) {
  localStorage.setItem(key, theme)
  const dark = theme === "dark" || theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches
  document.documentElement.classList.toggle("dark", dark)
}
