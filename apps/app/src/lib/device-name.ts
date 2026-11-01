// Suggested names from the user agent; the person can always edit them.
const ua = () => typeof navigator === 'undefined' ? '' : navigator.userAgent
function platform() {
  const value = ua()
  return /iPhone/.test(value) ? 'iPhone' : /iPad/.test(value) ? 'iPad' : /Android/.test(value) ? 'Android' : /Mac/.test(value) ? 'Mac' : /Windows/.test(value) ? 'Windows' : /Linux/.test(value) ? 'Linux' : '电脑'
}
function browser() {
  const value = ua()
  return /Edg\//.test(value) ? 'Edge' : /Firefox\//.test(value) ? 'Firefox' : /Chrome\//.test(value) ? 'Chrome' : /Safari\//.test(value) ? 'Safari' : '浏览器'
}
export const defaultBrowserName = () => `${platform()} 上的 ${browser()}`
export const defaultComputerName = () => `我的 ${platform()}`
const LAST_BROWSER_NAME = 'vibeface.browser-name'
/** A refused or removed browser asks again with the name the person last gave it. */
export function lastBrowserName() { try { return localStorage.getItem(LAST_BROWSER_NAME) } catch { return null } }
export function rememberBrowserName(name: string) { try { localStorage.setItem(LAST_BROWSER_NAME, name) } catch {} }
