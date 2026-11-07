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
import { ActivationQueue } from './activation-queue.js'
import { createClientHost, CLIENT_SCHEME_DESCRIPTOR } from './client-host.js'
import { CLIENT_ORIGIN } from '../shared/client-contract.js'
import { parseArgs } from './cli.js'
import { DesktopCoordinator, prepareAppProfileNode } from './desktop-coordinator.js'
import { runControl } from './control-client.js'
import { parseAccess, parseInspect } from './control-types.js'
import { deepLinkFromArgv, parseDeepLink, type DeepLink } from './deeplink.js'
import { isTrustedLauncherCall, readBoolean, readExternalUrl, readNoArguments, readString } from './ipc-guard.js'
import { registerLauncherScheme, serveLauncher } from './launcher-protocol.js'
import { applyApplicationMenu } from './menu.js'
import { cliBinary, coreBinary, runtimeRoot, controlProgram, runtimeState, serviceProgram } from './paths.js'
import { CoreSession } from './session.js'
import { ManagedNodeController } from './managed-node.js'
import { createTray } from './tray.js'
import {
  createClientWindow,
  createLauncherWindow,
  guardPermissions,
  launcherAssets,
  openExternally,
} from './windows.js'

const MAX_PATH = 1024

const mainDir = __dirname

let launcherWindow: BrowserWindow | null = null
let businessWindow: BrowserWindow | null = null
let tray: Tray | null = null
let quitting = false
let coreSession: CoreSession | null = null
let coordinator: DesktopCoordinator | null = null
let clientHost: ReturnType<typeof createClientHost> | null = null
let clientIpcInstalled = false
let shutdownPending = false

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
registerLauncherScheme([CLIENT_SCHEME_DESCRIPTOR])

function requireSession(): CoreSession {
  if (!coreSession) throw new Error('桌面会话还没有就绪')
  return coreSession
}

function businessOrigin(): string | null {
  return businessWindow && !businessWindow.isDestroyed() ? CLIENT_ORIGIN : null
}

function status(): LauncherStatus {
  const current = requireSession()
  const client = coordinator?.status() ?? {
    clientAvailable: true,
    nodeMode: 'undecided' as const,
    selectedNode: null,
    discovery: { kind: 'idle' as const },
    backgroundNode: { kind: 'checking' as const },
  }
  return {
    runtime: runtimeState(),
    core: current.state(),
    businessOpen: Boolean(businessWindow && !businessWindow.isDestroyed() && businessWindow.isVisible()),
    preferences: current.preferences(),
    profileDir,
    version: app.getVersion(),
    attachOnly: args.attachOnly,
    ...client,
    clientAvailable: clientHost !== null,
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

async function loadBusiness(target: string, replace = false): Promise<void> {
  if (replace && businessWindow && !businessWindow.isDestroyed()) {
    businessWindow.destroy()
    businessWindow = null
  }
  if (!businessWindow || businessWindow.isDestroyed()) {
    businessWindow = createClientWindow(mainDir)
    if (!clientIpcInstalled) {
      clientHost?.installIpc()
      clientIpcInstalled = true
    }
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
}

/**
 * The first-party client always opens, independently of the optional Node host.
 */
async function openBusiness(reconnect = false): Promise<void> {
  if (!clientHost) throw new Error('客户端界面尚未就绪。')
  if (!reconnect && businessWindow && !businessWindow.isDestroyed()) {
    businessWindow.show()
    businessWindow.focus()
    broadcast()
    return
  }
  const destination = new URL(clientHost.clientUrl)
  const selected = await requireCoordinator().resolveSelectedNode()
  if (selected && selected.access.kind === 'local') destination.hash = new URL(selected.access.url).hash
  await loadBusiness(destination.toString(), reconnect)
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
 * Keep the original invitation while the client stays at its fixed first-party origin.
 */
async function handleJoin(link: DeepLink): Promise<void> {
  if (link.kind !== 'join') return
  try {
    if (!clientHost) throw new Error('客户端界面尚未就绪。')
    const target = new URL('/join', clientHost.clientUrl)
    target.hash = new URLSearchParams({ invitation: link.origin === null ? link.ticket : `${link.origin}/join#${link.ticket}` }).toString()
    await loadBusiness(target.toString(), true)
  } catch (error) {
    reportFailure('无法打开加入链接', error)
  }
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

function stringArgument(args: readonly unknown[], what: string, maxLength: number): string {
  if (args.length !== 1) throw new TypeError(`${what} requires one argument`)
  return readString(args[0], what, maxLength)
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
    if (chosen) {
      await requireCoordinator().rememberDirectory(chosen)
      if (requireSession().preferences().openBusinessOnConnect) await openBusiness(true)
    }
    return status()
  })
  handle(IPC.useDataDir, async (passed) => {
    await requireCoordinator().rememberDirectory(stringArgument(passed, '数据目录', MAX_PATH))
    if (requireSession().preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.forgetDataDir, async (passed) => {
    readNoArguments(passed, IPC.forgetDataDir)
    await requireCoordinator().clearSelection()
    if (businessWindow && !businessWindow.isDestroyed()) await openBusiness(true)
    return status()
  })
  handle(IPC.startOwned, async (passed) => {
    readNoArguments(passed, IPC.startOwned)
    const current = requireSession()
    await requireCoordinator().startOwnedAppNode()
    if (current.preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.attachExisting, async (passed) => {
    readNoArguments(passed, IPC.attachExisting)
    const current = requireSession()
    await current.attachExisting()
    if (current.preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.stopOwned, async (passed) => {
    readNoArguments(passed, IPC.stopOwned)
    await requireSession().stopOwned()
    await requireCoordinator().discover()
    if (businessWindow && !businessWindow.isDestroyed()) await openBusiness(true)
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
  handle(IPC.discoverNodes, async (passed) => {
    readNoArguments(passed, IPC.discoverNodes)
    await requireCoordinator().discover()
    if (requireSession().preferences().openBusinessOnConnect && requireCoordinator().status().selectedNode) await openBusiness(true)
    return status()
  })
  handle(IPC.chooseNodeMode, async (passed) => {
    const mode = stringArgument(passed, '节点模式', 16)
    if (mode !== 'controller' && mode !== 'local') throw new TypeError('未知的节点模式')
    await requireCoordinator().chooseMode(mode)
    if (mode === 'controller') await openBusiness()
    return status()
  })
  handle(IPC.connectDiscoveredNode, async (passed) => {
    await requireCoordinator().connect(stringArgument(passed, '节点标识', 64))
    if (requireSession().preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.startDiscoveredNode, async (passed) => {
    await requireCoordinator().start(stringArgument(passed, '节点标识', 64))
    if (requireSession().preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.installBackgroundNode, async (passed) => {
    readNoArguments(passed, IPC.installBackgroundNode)
    await requireCoordinator().installBackground()
    await requireCoordinator().discover()
    if (requireSession().preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.startBackgroundNode, async (passed) => {
    readNoArguments(passed, IPC.startBackgroundNode)
    await requireCoordinator().startBackground()
    if (requireSession().preferences().openBusinessOnConnect) await openBusiness(true)
    return status()
  })
  handle(IPC.stopBackgroundNode, async (passed) => {
    readNoArguments(passed, IPC.stopBackgroundNode)
    await requireCoordinator().stopBackground()
    return status()
  })
  handle(IPC.removeBackgroundNode, async (passed) => {
    readNoArguments(passed, IPC.removeBackgroundNode)
    await requireCoordinator().removeBackground()
    return status()
  })
}

function requireCoordinator(): DesktopCoordinator {
  if (!coordinator) throw new Error('桌面节点协调器还没有就绪')
  return coordinator
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
  const managedNode = new ManagedNodeController({ profileDir, bundledCli: { command: cliBinary() } })

  const activations = new ActivationQueue(async (link) => {
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

  await managedNode.prepareProfile()
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
  coordinator = new DesktopCoordinator({
    profileDir,
    controlProgram: controlProgram(),
    managedNode,
    prepareBackgroundNode: () => prepareAppProfileNode(profileDir, controlProgram()),
    inspect: async (dataDir) => parseInspect(await runControl(controlProgram(), { command: 'inspect', data_dir: dataDir })),
    access: async (dataDir) => parseAccess(await runControl(controlProgram(), { command: 'access', data_dir: dataDir, json: true })),
    startDiscovered: async (node) => requireSession().startDiscoveredNode(node.dataDir, node.deviceId),
    startOwnedAppNode: async () => {
      await requireSession().useDataDir(join(profileDir, 'node'))
      await requireSession().startOwned()
    },
    beforeInstallBackground: async (node) => {
      const current = requireSession()
      const state = current.state()
      if (state.kind === 'owned') {
        if (state.dataDir !== node.dataDir) throw new Error('请先停止 Vibeface 启动的其他本机节点。')
        await current.stopOwned()
      } else if (node.status === 'running') {
        throw new Error('应用节点已由其他进程运行，停止它后才能安装后台服务。')
      }
    },
    onChange: broadcast,
  })
  clientHost = createClientHost({
    runtimeRoot: join(runtimeRoot(), 'web'),
    resolveSelectedNode: () => requireCoordinator().resolveSelectedNode(),
    clientBridgeProgram: coreBinary,
    getClientContents: () => businessWindow && !businessWindow.isDestroyed() ? businessWindow.webContents : null,
    showNodePanel: showLauncher,
  })
  clientHost.registerProtocol()

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
    event.preventDefault()
    if (shutdownPending) return
    shutdownPending = true
    quitting = true
    void (async () => { await coreSession?.shutdown(); await clientHost?.dispose() })().then(
      () => app.exit(0),
      (error: unknown) => {
        shutdownPending = false
        quitting = false
        reportFailure('暂时无法退出 Vibeface', error)
      }
    )
  })

  showLauncher()
  await coordinator.initialize()
  await coreSession.restore()
  broadcast()

  if (coordinator.status().nodeMode !== 'undecided' && coreSession.preferences().openBusinessOnConnect) {
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
