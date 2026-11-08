import { join } from 'node:path'
import { BrowserWindow, shell, type Event, type Session } from 'electron'
import { PRODUCT_NAME } from '../shared/contract.js'
import { CLIENT_HOST, CLIENT_ORIGIN, CLIENT_SCHEME } from '../shared/client-contract.js'

/** Permissions the Core-served page may ask for; everything else is refused. */
const BUSINESS_PERMISSIONS = new Set(['notifications', 'clipboard-sanitized-write', 'fullscreen', 'media'])

/** Open an http(s) link in the user's own browser; anything else is dropped. */
export function openExternally(rawUrl: string): void {
  try {
    const url = new URL(rawUrl)
    if (url.protocol === 'http:' || url.protocol === 'https:') void shell.openExternal(url.toString())
  } catch {
    /* an unparsable link is not opened */
  }
}

function guardNavigation(window: BrowserWindow, allowed: (url: string) => boolean): void {
  const navigate = (event: Event, url: string) => {
    if (allowed(url)) return
    event.preventDefault()
    openExternally(url)
  }
  window.webContents.on('will-navigate', navigate)
  window.webContents.on('will-redirect', navigate)
}

/** The client preload is given only to this fixed first-party asset origin. */
export function createClientWindow(mainDir: string): BrowserWindow {
  const window = new BrowserWindow({
    width: 1240, height: 820, minWidth: 720, minHeight: 540, show: false, title: PRODUCT_NAME,
    backgroundColor: '#ffffff',
    webPreferences: { preload: join(mainDir, '..', 'preload', 'client.cjs'), contextIsolation: true,
      nodeIntegration: false, sandbox: true, webSecurity: true, webviewTag: false },
  })
  window.webContents.setWindowOpenHandler(({ url }) => { openExternally(url); return { action: 'deny' } })
  guardNavigation(window, (url) => sameOrigin(url, CLIENT_ORIGIN))
  window.webContents.on('will-attach-webview', event => event.preventDefault())
  return window
}

export function sameOrigin(rawUrl: string, origin: string): boolean {
  try {
    const url = new URL(rawUrl)
    if (origin === CLIENT_ORIGIN) return url.protocol === `${CLIENT_SCHEME}:` && url.host === CLIENT_HOST && !url.username && !url.password
    return (url.protocol === 'http:' || url.protocol === 'https:') && url.origin === origin
  } catch {
    return false
  }
}

/** Permissions are answered per requesting origin for the sole client window. */
export function guardPermissions(session: Session, businessOrigin: () => string | null): void {
  session.setPermissionRequestHandler((contents, permission, callback, details) => {
    const origin = businessOrigin()
    const requesting = details.requestingUrl || contents?.getURL() || ''
    callback(Boolean(origin) && sameOrigin(requesting, origin ?? '') && BUSINESS_PERMISSIONS.has(permission))
  })
  session.setPermissionCheckHandler((_contents, permission, requestingOrigin) => {
    const origin = businessOrigin()
    return Boolean(origin) && sameOrigin(requestingOrigin, origin ?? '') && BUSINESS_PERMISSIONS.has(permission)
  })
  session.setDevicePermissionHandler(() => false)
}
