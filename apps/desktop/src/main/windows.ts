import { join } from 'node:path'
import { BrowserWindow, shell, type Session } from 'electron'
import { LAUNCHER_URL, PRODUCT_NAME } from '../shared/contract.js'

/** Permissions the Core-served page may ask for; everything else is refused. */
const BUSINESS_PERMISSIONS = new Set(['notifications', 'clipboard-sanitized-write', 'fullscreen', 'media'])

export function launcherPreload(mainDir: string): string {
  return join(mainDir, '..', 'preload', 'launcher.cjs')
}

export function launcherAssets(mainDir: string): string {
  return join(mainDir, '..', 'launcher')
}

/** Open an http(s) link in the user's own browser; anything else is dropped. */
export function openExternally(rawUrl: string): void {
  try {
    const url = new URL(rawUrl)
    if (url.protocol === 'http:' || url.protocol === 'https:') void shell.openExternal(url.toString())
  } catch {
    /* an unparsable link is not opened */
  }
}

export function createLauncherWindow(mainDir: string): BrowserWindow {
  const window = new BrowserWindow({
    width: 560,
    height: 600,
    minWidth: 460,
    minHeight: 420,
    show: false,
    title: `${PRODUCT_NAME} 启动器`,
    webPreferences: {
      preload: launcherPreload(mainDir),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
      webviewTag: false,
      spellcheck: false,
    },
  })
  window.setMenuBarVisibility(false)
  window.webContents.setWindowOpenHandler(({ url }) => {
    openExternally(url)
    return { action: 'deny' }
  })
  window.webContents.on('will-navigate', (event, url) => {
    if (url === LAUNCHER_URL) return
    event.preventDefault()
    openExternally(url)
  })
  window.webContents.on('will-attach-webview', (event) => event.preventDefault())
  void window.loadURL(LAUNCHER_URL)
  return window
}

export function createBusinessWindow(origin: string): BrowserWindow {
  const window = new BrowserWindow({
    width: 1240,
    height: 820,
    minWidth: 720,
    minHeight: 540,
    show: false,
    title: PRODUCT_NAME,
    backgroundColor: '#ffffff',
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
      webviewTag: false,
    },
  })
  window.webContents.setWindowOpenHandler(({ url }) => {
    openExternally(url)
    return { action: 'deny' }
  })
  window.webContents.on('will-navigate', (event, url) => {
    if (sameOrigin(url, origin)) return
    event.preventDefault()
    openExternally(url)
  })
  window.webContents.on('will-attach-webview', (event) => event.preventDefault())
  return window
}

export function sameOrigin(rawUrl: string, origin: string): boolean {
  try {
    return new URL(rawUrl).origin === origin
  } catch {
    return false
  }
}

/**
 * Permissions are answered per requesting origin: the Core page gets the browser
 * capabilities the web app already uses, and the launcher gets none at all.
 */
export function guardPermissions(session: Session, businessOrigin: () => string | null): void {
  session.setPermissionRequestHandler((contents, permission, callback, details) => {
    const origin = businessOrigin()
    const requesting = details.requestingUrl || contents?.getURL() || ''
    callback(Boolean(origin) && sameOrigin(requesting, origin ?? '') && BUSINESS_PERMISSIONS.has(permission))
  })
  session.setPermissionCheckHandler((_contents, permission, requestingOrigin) => {
    const origin = businessOrigin()
    return Boolean(origin) && requestingOrigin === origin && BUSINESS_PERMISSIONS.has(permission)
  })
  session.setDevicePermissionHandler(() => false)
}
