import { join } from 'node:path'
import { BrowserWindow, Tray, app, dialog, ipcMain, session, type IpcMainInvokeEvent } from 'electron'
import {
  APP_ID,
  DESKTOP_NODE_SETTINGS_PATH,
  IPC,
  PREFERENCE_KEYS,
  PRODUCT_NAME,
  type DesktopStatus,
  type PreferenceKey,
} from '../shared/contract.js'
import { ActivationQueue } from './activation-queue.js'
import { createClientHost, CLIENT_SCHEME_DESCRIPTOR } from './client-host.js'
import { CLIENT_ORIGIN, CLIENT_URL } from '../shared/client-contract.js'
import { parseArgs } from './cli.js'
import { registerDesktopSchemes } from './desktop-schemes.js'
import { DesktopCoordinator, prepareAppProfileNode } from './desktop-coordinator.js'
import { runControl } from './control-client.js'
import { parseAccess, parseInspect } from './control-types.js'
import { deepLinkFromArgv, parseDeepLink, type DeepLink } from './deeplink.js'
import { isTrustedClientCall, readBoolean, readExternalUrl, readNoArguments, readString } from './ipc-guard.js'
import { applyApplicationMenu } from './menu.js'
import { cliBinary, coreBinary, runtimeRoot, controlProgram, runtimeState, serviceProgram } from './paths.js'
import { CoreSession } from './session.js'
import { ManagedNodeController } from './managed-node.js'
import { createTray } from './tray.js'
import {
  createClientWindow,
  guardPermissions,
  openExternally,
} from './windows.js'
import { reloadClientContents } from './client-navigation.js'

const MAX_PATH = 1024

const mainDir = __dirname

let clientWindow: BrowserWindow | null = null
let tray: Tray | null = null
let quitting = false
let coreSession: CoreSession | null = null
let coordinator: DesktopCoordinator | null = null
let clientHost: ReturnType<typeof createClientHost> | null = null
let clientIpcInstalled = false
let shutdownPending = false
let hostPhase: DesktopStatus['hostPhase'] = 'starting'

function requireHostReady(): void {
  if (hostPhase !== 'ready') throw new Error('Vibeface 仍在启动，请稍后再试。')
}

function profileDirectory(explicit: string | null): string {
  if (explicit) return explicit
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
registerDesktopSchemes([CLIENT_SCHEME_DESCRIPTOR])

function requireSession(): CoreSession {
  if (!coreSession) throw new Error('桌面会话还没有就绪')
  return coreSession
}

function clientContents(): Electron.WebContents | null {
  return clientWindow && !clientWindow.isDestroyed() ? clientWindow.webContents : null
}

function status(): DesktopStatus {
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
    hostPhase,
    clientVisible: Boolean(clientWindow && !clientWindow.isDestroyed() && clientWindow.isVisible()),
    preferences: current.preferences(),
    profileDir,
    version: app.getVersion(),
    attachOnly: args.attachOnly,
    ...client,
    clientAvailable: clientHost !== null,
  }
}

function broadcast(): void {
  const live = clientContents()
  if (!coreSession || !live) return
  live.send(IPC.status, status())
}

async function navigateClient(path: string): Promise<void> {
  const contents = clientContents()
  if (!contents) return
  const escaped = JSON.stringify(path)
  await contents.executeJavaScript(
    `(function(){const p=${escaped};if(typeof window.__vibefaceNavigate==='function')window.__vibefaceNavigate(p);else if(location.pathname!==p)location.assign(new URL(p,location.origin).href)})()`
  )
}

async function openNodeSettings(): Promise<void> {
  await ensureClientWindow()
  clientWindow?.show()
  clientWindow?.focus()
  await navigateClient(DESKTOP_NODE_SETTINGS_PATH)
  broadcast()
}

async function focusClientApp(reconnect = false, showChat = true): Promise<void> {
  if (!clientHost) throw new Error('客户端界面尚未就绪。')
  await ensureClientWindow()
  const window = clientWindow!
  if (reconnect) {
    const destination = new URL(clientHost.clientUrl)
    destination.pathname = showChat ? '/' : DESKTOP_NODE_SETTINGS_PATH
    const selected = await requireCoordinator().resolveSelectedNode()
    if (selected && selected.access.kind === 'local') destination.hash = new URL(selected.access.url).hash
    clientHost.disconnect()
    await window.webContents.executeJavaScript(`history.replaceState(null, '', ${JSON.stringify(destination.toString())})`)
    await reloadClientContents(window.webContents)
  }
  window.show()
  window.focus()
  broadcast()
}

async function ensureClientWindow(): Promise<void> {
  if (!clientWindow || clientWindow.isDestroyed()) {
    clientWindow = createClientWindow(mainDir)
    if (!clientIpcInstalled) {
      clientHost?.installIpc()
      clientIpcInstalled = true
    }
    clientWindow.on('close', (event) => {
      if (quitting) return
      event.preventDefault()
      clientWindow?.hide()
      broadcast()
    })
    clientWindow.on('show', broadcast)
    clientWindow.on('hide', broadcast)
    clientWindow.webContents.on('did-finish-load', broadcast)
    await clientWindow.loadURL(clientHost?.clientUrl ?? CLIENT_URL)
  }
}

function focusClientSafely(): void {
  if (hostPhase !== 'ready') {
    void ensureClientWindow()
      .then(() => {
        clientWindow?.show()
        clientWindow?.focus()
      })
      .catch((error: unknown) => reportFailure('无法显示 Vibeface 窗口', error))
    return
  }
  void focusClientApp().catch((error: unknown) => reportFailure('无法显示 Vibeface 窗口', error))
}

function reportFailure(title: string, error: unknown): void {
  const detail = error instanceof Error ? error.message : '出现未知错误。'
  void openNodeSettings().catch(() => {})
  void dialog.showMessageBox({ type: 'warning', title, message: title, detail, buttons: ['好'] })
}

async function handleJoin(link: DeepLink): Promise<void> {
  if (link.kind !== 'join') return
  try {
    if (!clientHost) throw new Error('客户端界面尚未就绪。')
    const target = new URL('/join', clientHost.clientUrl)
    target.hash = new URLSearchParams({ invitation: link.origin === null ? link.ticket : `${link.origin}/join#${link.ticket}` }).toString()
    await ensureClientWindow()
    await clientWindow!.loadURL(target.toString())
    clientWindow!.show()
    clientWindow!.focus()
    broadcast()
  } catch (error) {
    reportFailure('无法打开加入链接', error)
  }
}

function trusted(event: Pick<IpcMainInvokeEvent, 'sender' | 'senderFrame'>): void {
  const frame = event.senderFrame
  let frameUrl: string | null = null
  let frameParent: object | null = null
  try {
    frameUrl = frame?.url ?? null
    frameParent = frame?.parent ?? null
  } catch {
    frameUrl = null
  }
  const live = clientContents()
  const ok = isTrustedClientCall(
    { sender: event.sender, senderFrame: frame, frameUrl, frameParent },
    { contents: live, mainFrame: live ? live.mainFrame : null }
  )
  if (!ok) throw new Error('这个请求不是来自受信任的客户端窗口')
}

function handle(channel: string, work: (args: readonly unknown[]) => Promise<unknown>): void {
  ipcMain.handle(channel, async (event, ...args: unknown[]) => {
    trusted(event)
    const before = coordinator?.status().selectedNode
    try {
      return await work(args)
    } finally {
      const after = coordinator?.status().selectedNode
      if (before?.deviceId !== after?.deviceId || before?.dataDir !== after?.dataDir) clientHost?.disconnect()
    }
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
  ipcMain.on(IPC.reconnectClient, (event, ...args: unknown[]) => {
    let showChat: boolean
    try {
      trusted(event)
      if (hostPhase !== 'ready') return
      if (args.length !== 1) return
      showChat = readBoolean(args[0], '显示聊天')
    } catch { return }
    void focusClientApp(true, showChat).catch((error: unknown) => reportFailure('无法重新连接本机节点', error))
  })
  handle(IPC.getStatus, async (passed) => {
    readNoArguments(passed, IPC.getStatus)
    return status()
  })
  handle(IPC.chooseDataDir, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.chooseDataDir)
    const picked = await dialog.showOpenDialog({
      title: '选择 Core 数据目录',
      properties: ['openDirectory', 'createDirectory'],
    })
    const chosen = picked.canceled ? null : picked.filePaths[0]
    if (chosen) {
      await requireCoordinator().rememberDirectory(chosen)
    }
    return status()
  })
  handle(IPC.useDataDir, async (passed) => {
    requireHostReady()
    await requireCoordinator().rememberDirectory(stringArgument(passed, '数据目录', MAX_PATH))
    return status()
  })
  handle(IPC.forgetDataDir, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.forgetDataDir)
    await requireCoordinator().clearSelection()
    return status()
  })
  handle(IPC.startOwned, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.startOwned)
    await requireCoordinator().startOwnedAppNode()
    return status()
  })
  handle(IPC.attachExisting, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.attachExisting)
    const current = requireSession()
    await current.attachExisting()
    return status()
  })
  handle(IPC.stopOwned, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.stopOwned)
    await requireSession().stopOwned()
    await requireCoordinator().discover()
    return status()
  })
  handle(IPC.focusClient, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.focusClient)
    await focusClientApp()
    return status()
  })
  handle(IPC.setPreference, async (passed) => {
    requireHostReady()
    await requireSession().setPreference(preferenceKey(passed[0]), readBoolean(passed[1], '设置值'))
    return status()
  })
  handle(IPC.openExternal, async (passed) => {
    openExternally(readExternalUrl(passed[0]))
  })
  handle(IPC.discoverNodes, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.discoverNodes)
    await requireCoordinator().refreshBackground()
    await requireCoordinator().discover()
    return status()
  })
  handle(IPC.chooseNodeMode, async (passed) => {
    requireHostReady()
    const mode = stringArgument(passed, '节点模式', 16)
    if (mode !== 'controller' && mode !== 'local') throw new TypeError('未知的节点模式')
    await requireCoordinator().chooseMode(mode)
    return status()
  })
  handle(IPC.connectDiscoveredNode, async (passed) => {
    requireHostReady()
    await requireCoordinator().connect(stringArgument(passed, '节点标识', 64))
    return status()
  })
  handle(IPC.startDiscoveredNode, async (passed) => {
    requireHostReady()
    await requireCoordinator().start(stringArgument(passed, '节点标识', 64))
    return status()
  })
  handle(IPC.installBackgroundNode, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.installBackgroundNode)
    await requireCoordinator().installBackground()
    await requireCoordinator().discover()
    return status()
  })
  handle(IPC.startBackgroundNode, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.startBackgroundNode)
    await requireCoordinator().startBackground()
    return status()
  })
  handle(IPC.stopBackgroundNode, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.stopBackgroundNode)
    await requireCoordinator().stopBackground()
    return status()
  })
  handle(IPC.removeBackgroundNode, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.removeBackgroundNode)
    await requireCoordinator().removeBackground()
    return status()
  })
  handle(IPC.updateBackgroundNode, async (passed) => {
    requireHostReady()
    readNoArguments(passed, IPC.updateBackgroundNode)
    await requireCoordinator().updateBackground()
    return status()
  })
}

function requireCoordinator(): DesktopCoordinator {
  if (!coordinator) throw new Error('桌面节点协调器还没有就绪')
  return coordinator
}

function installDownloadPrompt(): void {
  session.defaultSession.on('will-download', (_event, item) => {
    const window = BrowserWindow.getFocusedWindow() ?? clientWindow
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
    if (activations.isReady()) focusClientSafely()
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

  guardPermissions(session.defaultSession, () => CLIENT_ORIGIN)
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
    getClientContents: clientContents,
    showNodePanel: openNodeSettings,
  })
  clientHost.registerProtocol()

  applyApplicationMenu({ openNodeSettings, focusClient: focusClientSafely, quit })
  tray = createTray({ openNodeSettings, focusClient: focusClientSafely, quit })

  registerIpc()

  app.setAsDefaultProtocolClient('vibeface')

  app.on('activate', () => {
    if (hostPhase !== 'ready') {
      void ensureClientWindow()
        .then(() => {
          clientWindow?.show()
          clientWindow?.focus()
        })
        .catch((error: unknown) => reportFailure('无法显示 Vibeface 窗口', error))
      return
    }
    if (clientWindow && !clientWindow.isDestroyed() && clientWindow.isVisible()) clientWindow.focus()
    else focusClientSafely()
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

  await ensureClientWindow()
  await navigateClient(DESKTOP_NODE_SETTINGS_PATH)
  clientWindow?.show()
  broadcast()

  await coordinator.initialize()
  await coreSession.restore()
  broadcast()

  if (coordinator.status().nodeMode === 'undecided') {
    await navigateClient(DESKTOP_NODE_SETTINGS_PATH)
  } else if (coreSession.preferences().openBusinessOnConnect) {
    await focusClientApp(true).catch(() => {
      /* node settings explain why the client could not open */
    })
  } else {
    await navigateClient(DESKTOP_NODE_SETTINGS_PATH)
  }

  hostPhase = 'ready'
  broadcast()
  activations.setReady()
  await activations.flush()
}

void start().catch((error: unknown) => {
  const detail = error instanceof Error ? error.message : String(error)
  dialog.showErrorBox(`${PRODUCT_NAME} 启动失败`, detail)
  app.exit(1)
})
