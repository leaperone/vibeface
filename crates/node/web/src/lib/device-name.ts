// Suggested names from the user agent; the person can always edit them.
const ua = () => typeof navigator === 'undefined' ? '' : navigator.userAgent
export function platform() {
  const value = ua()
  return /iPhone/.test(value) ? 'iPhone' : /iPad/.test(value) ? 'iPad' : /Android/.test(value) ? 'Android' : /Mac/.test(value) ? 'Mac' : /Windows/.test(value) ? 'Windows' : /Linux/.test(value) ? 'Linux' : '电脑'
}
function browser() {
  const value = ua()
  return /Edg\//.test(value) ? 'Edge' : /Firefox\//.test(value) ? 'Firefox' : /Chrome\//.test(value) ? 'Chrome' : /Safari\//.test(value) ? 'Safari' : '浏览器'
}
/** `deviceId` tells apart two browsers of the same kind joining one network. */
export const defaultBrowserName = (deviceId = '') => `${platform()} 上的 ${browser()}${deviceId ? ` ${deviceId.slice(0, 4).toUpperCase()}` : ''}`
export const defaultComputerName = () => `我的 ${platform()}`
