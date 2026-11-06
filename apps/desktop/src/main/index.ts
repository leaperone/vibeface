import { existsSync } from 'node:fs'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import {
  Notification,
  app,
  BrowserWindow,
  type DownloadItem,
  type WebContents,
  clipboard,
  dialog,
  ipcMain,
  Menu,
  Tray,
  nativeImage,
  shell,
  session,
} from 'electron'
import { AppState } from './app-state.js'
import { parseDeepLink } from './deeplink.js'
import { normalizeAccessUrl, parseLocalListen } from './access-url.js'
import { runControl } from './control-client.js'
import type { ControlRequest } from './control-types.js'
import { coreBinary, webRoot } from './paths.js'
import { CoreLifecycle } from './lifecycle.js'
import { startWebStatic } from './web-server.js'

const APP_ID = 'one.leaper.vibeface.desktop'

let quitting = false
let tray: Tray | null = null
let launcherWin: BrowserWindow | null = null
let businessWin: BrowserWindow | null = null
let launchWeb: { port: number; close(): Promise<void> } | null = null

const lifecycle = new CoreLifecycle()
let appState: AppState

function profileDir(): string {
  const env = process.env.VIBEFACE_DESKTOP_PROFILE_DIR?.trim()
  if (env) return env
  const i = process.argv.indexOf('--profile-dir')
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1]!
  return app.getPath('userData')
}

function attachFlag(): boolean {
  return process.argv.includes('--attach')
}

function preloadPath(name: 'launcher' | 'business'): string {
  return join(__dirname, '..', 'preload', `${name}.cjs`)
}

async function loadSettings(dataDir: string): Promise<{ lastDataDir?: string }> {
  const path = join(dataDir, 'settings.json')
  try {
    const raw = await readFile(path, 'utf8')
    return JSON.parse(raw) as { lastDataDir?: string }
  } catch {
    return {}
  }
}

async function saveSettings(dir: string, data: { lastDataDir?: string }) {
  await mkdir(dir, { recursive: true })
  await writeFile(join(dir, 'settings.json'), JSON.stringify(data, null, 2), 'utf8')
}

function broadcastLauncherStatus() {
  const payload = {
    mode: appState.connectionMode(lifecycle),
    dataDir: appState.dataDir,
    businessReady: Boolean(appState.businessOrigin),
    deviceId: appState.deviceId,
    attach: attachFlag(),
  }
  launcherWin?.webContents.send('launcher:status', payload)
}

async function pickDataDir(): Promise<string | null> {
  const r = await dialog.showOpenDialog({
    title: '选择 Core 数据目录',
    properties: ['openDirectory', 'createDirectory'],
  })
  if (r.canceled || !r.filePaths[0]) return null
  return r.filePaths[0]
}

async function probeInspect(dataDir: string): Promise<unknown> {
  const core = coreBinary()
  const request: ControlRequest = { command: 'inspect', data_dir: dataDir }
  return runControl(core, request)
}

async function ensureBusinessWindow(): Promise<BrowserWindow> {
  if (businessWin && !businessWin.isDestroyed()) return businessWin
  businessWin = new BrowserWindow({
    width: 1280,
    height: 820,
    show: false,
    title: 'Vibeface',
    webPreferences: {
      preload: preloadPath('business'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  })
  businessWin.webContents.session.setPermissionRequestHandler(
    (_wc: WebContents, permission: string, callback: (allow: boolean) => void) => {
      const origins = new Set(['clipboard-read', 'clipboard-sanitized-write', 'notifications', 'media'])
      callback(origins.has(permission))
    }
  )
  businessWin.webContents.setWindowOpenHandler(({ url }: { url: string }) => {
    void shell.openExternal(url)
    return { action: 'deny' }
  })
  businessWin.webContents.on('will-navigate', (event: unknown, ...args: unknown[]) => {
    const url = args[0]
    if (typeof url !== 'string') return
    const origin = appState.businessOrigin
    if (!origin || !url.startsWith(origin)) {
      ;(event as { preventDefault: () => void }).preventDefault()
    }
  })
  businessWin.on('close', (event: unknown) => {
    if (!quitting) {
      ;(event as { preventDefault: () => void }).preventDefault()
      businessWin?.hide()
    }
  })
  return businessWin
}

async function loadBusinessUrl(url: string): Promise<void> {
  const win = await ensureBusinessWindow()
  await win.loadURL(url)
  win.show()
  win.focus()
}

async function openBusinessFromCore(): Promise<void> {
  const core = coreBinary()
  const dir = appState.dataDir
  if (!dir) throw new Error('data dir required')

  const inspectRaw = await probeInspect(dir)
  const inspect = inspectRaw as { local_listen?: string; device_id?: string }
  appState.setBusinessFromInspect(inspect)

  const accessRaw = await lifecycle.accessUrl(core, false)
  const access = accessRaw as { url?: string }
  if (typeof access.url !== 'string') throw new Error('access url missing')

  const ll = typeof inspect.local_listen === 'string' ? parseLocalListen(inspect.local_listen) : null
  let target = access.url
  if (ll) target = normalizeAccessUrl(access.url, ll)

  if (existsSync(webRoot())) {
    if (launchWeb) await launchWeb.close().catch(() => {})
    launchWeb = await startWebStatic(webRoot())
    const u = new URL(target)
    u.protocol = 'http:'
    u.hostname = '127.0.0.1'
    u.port = String(launchWeb.port)
    target = u.toString()
  }

  await loadBusinessUrl(target)
}

function ensureLauncherWindow(): BrowserWindow {
  if (launcherWin && !launcherWin.isDestroyed()) return launcherWin
  launcherWin = new BrowserWindow({
    width: 520,
    height: 480,
    title: 'Vibeface 桌面',
    show: false,
    webPreferences: {
      preload: preloadPath('launcher'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
    },
  })
  const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"/><meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'unsafe-inline';"/><style>
    body{font-family:system-ui,-apple-system,sans-serif;margin:16px;background:#0c0c0d;color:#e8e8ea}
    h1{font-size:16px;font-weight:600;margin:0 0 12px}
    button,input{width:100%;padding:8px;margin:6px 0;box-sizing:border-box;border-radius:8px;border:1px solid #333;background:#1a1a1d;color:inherit}
    button.primary{background:#3b82f6;border-color:#2563eb;color:#fff}
    .row{display:flex;gap:8px}
    .row button{flex:1}
    #log{white-space:pre-wrap;font-size:12px;opacity:.85;min-height:80px;margin-top:12px}
  </style></head><body>
    <h1>Vibeface 桌面</h1>
    <p style="font-size:13px;opacity:.9">选择已有 Core 数据目录：可连接已在运行的服务，或由本应用启动托管实例。</p>
    <button type="button" id="pick">选择数据目录…</button>
    <input id="path" readonly placeholder="未选择"/>
    <div class="row">
      <button type="button" class="primary" id="attach">仅连接</button>
      <button type="button" id="start">启动本机 Core</button>
    </div>
    <div class="row">
      <button type="button" id="open">打开业务窗口</button>
      <button type="button" id="stop">停止本机 Core</button>
    </div>
    <div id="log"></div>
    <script>
      const log = (m) => { const el = document.getElementById('log'); el.textContent = typeof m === 'string' ? m : JSON.stringify(m, null, 2) }
      const pathEl = document.getElementById('path')
      window.vibefaceLauncher.onStatus((s) => log(s))
      document.getElementById('pick').onclick = async () => {
        const p = await window.vibefaceLauncher.pickDataDir()
        if (p) pathEl.value = p
      }
      document.getElementById('attach').onclick = async () => {
        const p = pathEl.value.trim()
        if (!p) return log('先选择数据目录')
        try {
          const info = await window.vibefaceLauncher.probeInspect(p)
          log(info)
          await window.vibefaceLauncher.attach(p)
        } catch (e) { log(String(e)) }
      }
      document.getElementById('start').onclick = async () => {
        const p = pathEl.value.trim()
        if (!p) return log('先选择数据目录')
        try { await window.vibefaceLauncher.startOwned(p) } catch (e) { log(String(e)) }
      }
      document.getElementById('open').onclick = async () => {
        try { const u = await window.vibefaceLauncher.getAccessUrl(); log(u || '(null)') } catch (e) { log(String(e)) }
      }
      document.getElementById('stop').onclick = async () => {
        try { await window.vibefaceLauncher.stopOwned(); log('stopped') } catch (e) { log(String(e)) }
      }
    </script>
  </body></html>`
  launcherWin.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent(html))
  launcherWin.on('close', (event: unknown) => {
    if (!quitting) {
      ;(event as { preventDefault: () => void }).preventDefault()
      launcherWin?.hide()
    }
  })
  return launcherWin
}

function showLauncher() {
  const win = ensureLauncherWindow()
  win.show()
  win.focus()
  broadcastLauncherStatus()
}

function createTray(): void {
  const iconPath = join(__dirname, '../../assets/tray.png')
  const icon = existsSync(iconPath) ? nativeImage.createFromPath(iconPath) : nativeImage.createEmpty()
  tray = new Tray(icon)
  tray.setToolTip('Vibeface')
  tray.on('click', () => showLauncher())
}

function applyMenu(): void {
  const template: Record<string, unknown>[] = [
    {
      label: 'Vibeface',
      submenu: [
        { role: 'about' },
        { type: 'separator' },
        {
          label: '打开启动器',
          click: () => showLauncher(),
        },
        {
          label: '检查更新…',
          click: () =>
            void shell.openExternal('https://github.com/leaperone/vibeface/releases'),
        },
        { type: 'separator' },
        { role: 'quit' },
      ],
    },
    {
      label: '编辑',
      submenu: [{ role: 'undo' }, { role: 'redo' }, { type: 'separator' }, { role: 'cut' }, { role: 'copy' }, { role: 'paste' }],
    },
    {
      label: '窗口',
      submenu: [{ role: 'minimize' }, { role: 'close' }],
    },
  ]
  Menu.setApplicationMenu(Menu.buildFromTemplate(template))
}

function forwardJoinLink(link: string) {
  const parsed = parseDeepLink(link)
  if (parsed.kind === 'join') {
    void clipboard.writeText(parsed.link)
    ensureBusinessWindow()
      .then((w) => {
        w.webContents.send('business:deeplink', parsed.link)
        if (Notification.isSupported()) {
          const n = new Notification({
            title: '收到加入链接',
            body: '链接已复制；可在业务窗口粘贴到加入流程。',
          })
          n.show()
        }
        void dialog.showMessageBox({
          type: 'info',
          message: '收到加入链接',
          detail: '链接已复制到剪贴板。请在业务窗口继续完成加入。',
        })
      })
      .catch(() => {})
  }
}

async function main(): Promise<void> {
  app.setAppUserModelId(APP_ID)

  const gotLock = app.requestSingleInstanceLock()
  if (!gotLock) {
    app.quit()
    return
  }

  const prof = profileDir()
  appState = new AppState(prof)

  await app.whenReady()

  const settings = await loadSettings(prof)
  if (settings.lastDataDir) appState.dataDir = settings.lastDataDir

  applyMenu()
  createTray()

  session.defaultSession.on('will-download', async (_event: unknown, ...args: unknown[]) => {
    const item = args[0] as DownloadItem
    const win = BrowserWindow.getFocusedWindow()
    const defaultPath = join(app.getPath('downloads'), item.getFilename())
    const saved = await dialog.showSaveDialog(win ?? undefined, { defaultPath })
    if (!saved.canceled && saved.filePath) item.setSavePath(saved.filePath)
  })

  ipcMain.handle('launcher:pick-data-dir', pickDataDir)
  ipcMain.handle('launcher:probe-inspect', async (_e: unknown, ...args: unknown[]) => {
    const dataDir = args[0]
    if (typeof dataDir !== 'string') throw new TypeError('data directory required')
    return probeInspect(dataDir)
  })
  ipcMain.handle('launcher:attach', async (_e: unknown, ...args: unknown[]) => {
    const dataDir = args[0]
    if (typeof dataDir !== 'string') throw new TypeError('data directory required')
    appState.dataDir = dataDir
    await saveSettings(appState.profileDir, { lastDataDir: dataDir })
    lifecycle.attachExisting(dataDir)
    const info = (await probeInspect(dataDir)) as { status?: string }
    if (info.status !== 'running') throw new Error('该数据目录对应的 Core 未在运行，无法仅连接')
    appState.setBusinessFromInspect(info as { local_listen?: string; device_id?: string })
    broadcastLauncherStatus()
  })
  ipcMain.handle('launcher:start-owned', async (_e: unknown, ...args: unknown[]) => {
    const dataDir = args[0]
    if (typeof dataDir !== 'string') throw new TypeError('data directory required')
    appState.dataDir = dataDir
    await saveSettings(appState.profileDir, { lastDataDir: dataDir })
    const fakeCore = process.env.VIBEFACE_DESKTOP_FAKE_CORE_EXE?.trim()
    const core = fakeCore && existsSync(fakeCore) ? fakeCore : coreBinary()
    const coreArgs: string[] = fakeCore ? [] : [`--data-dir`, dataDir]
    const wr = webRoot()
    if (!fakeCore && existsSync(wr)) coreArgs.push(`--app-dir`, wr)
    await lifecycle.spawnOwned(core, dataDir, coreArgs, { VIBEFACE_DATA_DIR: dataDir })
    broadcastLauncherStatus()
  })
  ipcMain.handle('launcher:stop-owned', async () => {
    await lifecycle.stopOwned('quit')
    broadcastLauncherStatus()
  })
  ipcMain.handle('launcher:get-access-url', async () => {
    await openBusinessFromCore()
    broadcastLauncherStatus()
    return appState.businessOrigin
  })
  ipcMain.handle('shell:open-external', async (_e: unknown, ...args: unknown[]) => {
    const url = args[0]
    if (typeof url !== 'string') throw new TypeError('url required')
    const u = new URL(url)
    if (u.protocol !== 'http:' && u.protocol !== 'https:') throw new Error('blocked URL protocol')
    await shell.openExternal(url)
  })
  ipcMain.handle('business:notify', async (_e: unknown, ...args: unknown[]) => {
    const payload = args[0]
    if (!payload || typeof payload !== 'object') throw new TypeError('payload required')
    const p = payload as { title: string; body: string; tag?: string }
    if (!Notification.isSupported()) return
    const n = new Notification({ title: p.title, body: p.body })
    n.show()
  })
  ipcMain.handle('business:request-show', async () => {
    const w = await ensureBusinessWindow()
    w.show()
    w.focus()
  })

  app.on('second-instance', (_event: unknown, argv: unknown) => {
    showLauncher()
    if (!Array.isArray(argv)) return
    const cand = argv.find(
      (a): a is string =>
        typeof a === 'string' && (a.startsWith('vibeface://') || (a.includes('/join#') && a.startsWith('http')))
    )
    if (cand) forwardJoinLink(cand)
  })

  app.on('open-url', (event: unknown, url: unknown) => {
    ;(event as { preventDefault: () => void }).preventDefault()
    if (typeof url === 'string') forwardJoinLink(url)
  })

  app.on('activate', () => {
    showLauncher()
  })

  app.on('before-quit', async (e: unknown) => {
    if (quitting) return
    ;(e as { preventDefault: () => void }).preventDefault()
    quitting = true
    await lifecycle.stopOwned('quit')
    if (launchWeb) await launchWeb.close().catch(() => {})
    app.exit(0)
  })

  process.on('uncaughtException', () => {
    void lifecycle.stopOwned('quit').finally(() => process.exit(1))
  })

  app.on('window-all-closed', () => {
    /* dock / tray keeps process */
  })

  if (app.isPackaged) app.setAsDefaultProtocolClient('vibeface')

  if (attachFlag() && settings.lastDataDir) {
    try {
      lifecycle.attachExisting(settings.lastDataDir)
      appState.dataDir = settings.lastDataDir
      const info = (await probeInspect(settings.lastDataDir)) as { local_listen?: string; device_id?: string }
      appState.setBusinessFromInspect(info)
      await openBusinessFromCore()
    } catch {
      showLauncher()
    }
  } else {
    showLauncher()
  }

  const startUrl = process.argv.find((a) => a.startsWith('vibeface://') || (a.includes('/join#') && a.startsWith('http')))
  if (startUrl) forwardJoinLink(startUrl)
}

void main()
