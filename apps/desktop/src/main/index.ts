import { join } from 'node:path'
import { BrowserWindow, Tray, app, dialog, ipcMain, session, type IpcMainInvokeEvent } from 'electron'
import {
  APP_ID,
  IPC,
  PREFERENCE_KEYS,
  PRODUCT_NAME,
  type LauncherStatus,
  type PreferenceKey,
} from '../shared/contract.js'
import { ActivationQueue, type JoinActivation } from './activation-queue.js'
import { parseArgs } from './cli.js'
import { deepLinkFromArgv, isLoopbackOrigin, parseDeepLink, type DeepLink } from './deeplink.js'
import { isTrustedLauncherCall, readBoolean, readExternalUrl, readNoArguments, readString } from './ipc-guard.js'
import { registerLauncherScheme, serveLauncher } from './launcher-protocol.js'
import { applyApplicationMenu } from './menu.js'
import { controlProgram, runtimeState, serviceProgram } from './paths.js'
import { CoreSession } from './session.js'
import { createTray } from './tray.js'
import {
  createBusinessWindow,
  createLauncherWindow,
  guardPermissions,
  launcherAssets,
  openExternally,
  sameOrigin,
} from './windows.js'

const MAX_PATH = 1024
const LOAD_TIMEOUT_MS = 20_000

const mainDir = __dirname

let launcherWindow: BrowserWindow | null = null
let businessWindow: BrowserWindow | null = null
let tray: Tray | null = null
let quitting = false
let coreSession: CoreSession | null = null

function profileDirectory(explicit: string | null): string {
  if (explicit) return explicit
  // Desktop state is device-local, so Windows uses LOCALAPPDATA rather than the roaming profile.
  if (process.platform === 'win32') {
    return join(process.env['LOCALAPPDATA'] ?? app.getPath('appData'), 'Vibeface', 'Desktop')
  }
  if (process.platform === 'darwin') return join(app.getPath('appData'), 'Vibeface Desktop')
  return join(app.getPath('appData'), 'vibeface-desktop')
}

const args = parseArgs(process.argv.slice(1))
const profileDir = profileDirectory(args.profileDir)

app.setName(PRODUCT_NAME)
app.setAppUserModelId(APP_ID)
app.setPath('userData', profileDir)
app.setPath('sessionData', join(profileDir, 'chromium'))
registerLauncherScheme()

function requireSession(): CoreSession {
  if (!coreSession) throw new Error('桌面会话还没有就绪')
  return coreSession
}

function businessOrigin(): string | null {
  const state = coreSession?.state()
  if (!state) return null
  return (state.kind === 'owned' || state.kind === 'attached') && state.origin ? state.origin : null
}

function status(): LauncherStatus {
  const current = requireSession()
  return {
    runtime: runtimeState(),
    core: current.state(),
    businessOpen: Boolean(businessWindow && !businessWindow.isDestroyed() && businessWindow.isVisible()),
    preferences: current.preferences(),
    profileDir,
    version: app.getVersion(),
    attachOnly: args.attachOnly,
  }
}

function broadcast(): void {
  if (!coreSession || !launcherWindow || launcherWindow.isDestroyed()) return
  launcherWindow.webContents.send(IPC.status, status())
}

function showLauncher(): void {
  if (!launcherWindow || launcherWindow.isDestroyed()) {
    launcherWindow = createLauncherWindow(mainDir)
    launcherWindow.on('ready-to-show', () => launcherWindow?.show())
    launcherWindow.on('close', (event) => {
      if (quitting) return
      event.preventDefault()
      launcherWindow?.hide()
    })
    launcherWindow.webContents.on('did-finish-load', broadcast)
    return
  }
  launcherWindow.show()
  launcherWindow.focus()
  broadcast()
}

async function loadBusiness(target: string): Promise<BrowserWindow> {
  const origin = new URL(target).origin
  if (businessWindow && !businessWindow.isDestroyed() && !sameOrigin(businessWindow.webContents.getURL(), origin)) {
    businessWindow.destroy()
    businessWindow = null
  }
  if (!businessWindow || businessWindow.isDestroyed()) {
    businessWindow = createBusinessWindow(origin)
    businessWindow.on('close', (event) => {
      if (quitting) return
      event.preventDefault()
      businessWindow?.hide()
      broadcast()
    })
    businessWindow.on('show', broadcast)
    businessWindow.on('hide', broadcast)
  }
  const window = businessWindow
  await window.loadURL(target)
  window.show()
  window.focus()
  broadcast()
  return window
}

/**
 * Open the UI Core serves at its own private origin. An invite continues in the same window
 * on that origin, after the access URL has established the local session.
 */
async function openBusiness(invite?: { ticket: string }): Promise<void> {
  const target = await requireSession().businessTarget()
  const origin = new URL(target).origin
  const connected =
    businessWindow && !businessWindow.isDestroyed() && sameOrigin(businessWindow.webContents.getURL(), origin)
  if (invite && connected) {
    await loadBusiness(`${origin}/join#${invite.ticket}`)
    return
  }
  const window = await loadBusiness(target)
  if (!invite) return
  await Promise.race([
    new Promise<void>((resolve) => window.webContents.once('did-finish-load', () => resolve())),
    new Promise<void>((resolve) => setTimeout(resolve, LOAD_TIMEOUT_MS)),
  ])
  await loadBusiness(`${origin}/join#${invite.ticket}`)
}

function openBusinessSafely(): void {
  void openBusiness().catch((error: unknown) => reportFailure('无法打开 Vibeface 窗口', error))
}

function reportFailure(title: string, error: unknown): void {
  const detail = error instanceof Error ? error.message : '出现未知错误。'
  showLauncher()
  void dialog.showMessageBox({ type: 'warning', title, message: title, detail, buttons: ['好'] })
}

/**
 * An invite published by this machine's own Core continues inside the desktop window. One
 * published by another entry belongs to that origin, which this window never navigates to,
 * so the user decides whether it opens in their browser.
 */
async function handleJoin(link: DeepLink): Promise<void> {
  if (link.kind !== 'join') return
  if (link.origin !== null && !isLoopbackOrigin(link.origin)) {
    showLauncher()
    const answer = await dialog.showMessageBox({
      type: 'question',
      title: '收到其他入口的加入链接',
      message: '这条加入链接指向另一个入口地址。',
      detail: `${link.origin}\n\n可以在系统浏览器中打开它继续加入流程。`,
      buttons: ['在浏览器中打开', '取消'],
      defaultId: 1,
      cancelId: 1,
    })
    if (answer.response === 0) openExternally(`${link.origin}/join#${link.ticket}`)
    return
  }
  try {
    await openBusiness({ ticket: link.ticket })
  } catch (error) {
    reportFailure('无法打开加入链接', error)
  }
}

function canHandleLocally(link: JoinActivation): boolean {
  if (link.origin !== null && !isLoopbackOrigin(link.origin)) return true
  const state = coreSession?.state()
  return state?.kind === 'owned' || state?.kind === 'attached'
}

function trusted(event: IpcMainInvokeEvent): void {
  const frame = event.senderFrame
  let frameUrl: string | null = null
  let frameParent: object | null = null
  try {
    frameUrl = frame?.url ?? null
    frameParent = frame?.parent ?? null
  } catch {
    frameUrl = null
  }
  const live = launcherWindow && !launcherWindow.isDestroyed() ? launcherWindow.webContents : null
  const ok = isTrustedLauncherCall(
    { sender: event.sender, senderFrame: frame, frameUrl, frameParent },
    { contents: live, mainFrame: live ? live.mainFrame : null }
  )
  if (!ok) throw new Error('这个请求不是来自受信任的启动器窗口')
}

function handle(channel: string, work: (args: readonly unknown[]) => Promise<unknown>): void {
  ipcMain.handle(channel, async (event, ...args: unknown[]) => {
    trusted(event)
    return work(args)
  })
}

function preferenceKey(value: unknown): PreferenceKey {
  const name = readString(value, '设置项', 64)
  const found = PREFERENCE_KEYS.find((key) => key === name)
  if (!found) throw new TypeError('未知的设置项')
  return found
}

function registerIpc(): void {
  handle(IPC.getStatus, async (passed) => {
    readNoArguments(passed, IPC.getStatus)
    return status()
  })
  handle(IPC.chooseDataDir, async (passed) => {
    readNoArguments(passed, IPC.chooseDataDir)
    const picked = await dialog.showOpenDialog({
      title: '选择 Core 数据目录',
      properties: ['openDirectory', 'createDirectory'],
    })
    const chosen = picked.canceled ? null : picked.filePaths[0]
    if (chosen) await requireSession().useDataDir(chosen)
    return status()
  })
  handle(IPC.useDataDir, async (passed) => {
    await requireSession().useDataDir(readString(passed[0], '数据目录', MAX_PATH))
    return status()
  })
  handle(IPC.forgetDataDir, async (passed) => {
    readNoArguments(passed, IPC.forgetDataDir)
    await requireSession().forgetDataDir()
    return status()
  })
  handle(IPC.startOwned, async (passed) => {
    readNoArguments(passed, IPC.startOwned)
    const current = requireSession()
    await current.startOwned()
    if (current.preferences().openBusinessOnConnect) await openBusiness()
    return status()
  })
  handle(IPC.attachExisting, async (passed) => {
    readNoArguments(passed, IPC.attachExisting)
    const current = requireSession()
    await current.attachExisting()
    if (current.preferences().openBusinessOnConnect) await openBusiness()
    return status()
  })
  handle(IPC.stopOwned, async (passed) => {
    readNoArguments(passed, IPC.stopOwned)
    await requireSession().stopOwned()
    return status()
  })
  handle(IPC.openBusiness, async (passed) => {
    readNoArguments(passed, IPC.openBusiness)
    await openBusiness()
    return status()
  })
  handle(IPC.setPreference, async (passed) => {
    await requireSession().setPreference(preferenceKey(passed[0]), readBoolean(passed[1], '设置值'))
    return status()
  })
  handle(IPC.openExternal, async (passed) => {
    openExternally(readExternalUrl(passed[0]))
  })
}

/** A download always asks where it goes; the page never writes to disk on its own. */
function installDownloadPrompt(): void {
  session.defaultSession.on('will-download', (_event, item) => {
    const window = BrowserWindow.getFocusedWindow() ?? businessWindow
    const options = { title: '保存文件', defaultPath: join(app.getPath('downloads'), item.getFilename()) }
    const target = window ? dialog.showSaveDialogSync(window, options) : dialog.showSaveDialogSync(options)
    if (target) item.setSavePath(target)
    else item.cancel()
  })
}

function quit(): void {
  if (quitting) return
  quitting = true
  app.quit()
}

async function start(): Promise<void> {
  if (!app.requestSingleInstanceLock()) {
    app.exit(0)
    return
  }

  const activations = new ActivationQueue(async (link) => {
    if (!canHandleLocally(link)) {
      showLauncher()
      return 'defer'
    }
    await handleJoin(link)
    return 'handled'
  })
  activations.enqueue(deepLinkFromArgv(process.argv.slice(1)))

  app.on('second-instance', (_event, argv) => {
    const passed = Array.isArray(argv) ? argv.filter((value): value is string => typeof value === 'string') : []
    const parsed = deepLinkFromArgv(passed)
    if (parsed.kind === 'join') {
      activations.enqueue(parsed)
      void activations.flush()
      return
    }
    if (activations.isReady()) showLauncher()
  })

  app.on('open-url', (event, url) => {
    event.preventDefault()
    const parsed = parseDeepLink(url)
    if (parsed.kind !== 'join') return
    activations.enqueue(parsed)
    void activations.flush()
  })

  await app.whenReady()

  serveLauncher(launcherAssets(mainDir))
  guardPermissions(session.defaultSession, businessOrigin)
  installDownloadPrompt()

  coreSession = new CoreSession(
    { runtime: runtimeState, control: controlProgram, service: serviceProgram },
    {
      profileDir,
      attachOnly: args.attachOnly,
      onChange: () => {
        broadcast()
        void activations.flush()
      },
    }
  )

  applyApplicationMenu({ showLauncher, openBusiness: openBusinessSafely, quit })
  tray = createTray({ showLauncher, openBusiness: openBusinessSafely, quit })

  registerIpc()
  app.setAsDefaultProtocolClient('vibeface')

  app.on('activate', () => {
    if (businessWindow && !businessWindow.isDestroyed() && businessWindow.isVisible()) businessWindow.focus()
    else showLauncher()
  })

  app.on('window-all-closed', () => {
    /* the tray keeps the app running; quitting is explicit */
  })

  app.on('before-quit', (event) => {
    quitting = true
    if (!coreSession?.ownsCore()) return
    event.preventDefault()
    void coreSession.shutdown().then(
      () => app.exit(0),
      (error: unknown) => {
        quitting = false
        reportFailure('暂时无法退出 Vibeface', error)
      }
    )
  })

  showLauncher()
  await coreSession.restore()
  broadcast()

  const state = coreSession.state()
  if (coreSession.preferences().openBusinessOnConnect && (state.kind === 'attached' || state.kind === 'owned')) {
    await openBusiness().catch(() => {
      /* the launcher already shows why the window could not open */
    })
  }

  activations.setReady()
  await activations.flush()
}

void start().catch((error: unknown) => {
  const detail = error instanceof Error ? error.message : String(error)
  dialog.showErrorBox(`${PRODUCT_NAME} 启动失败`, detail)
  app.exit(1)
})
