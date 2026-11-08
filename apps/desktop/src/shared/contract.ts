/** Types shared by the main process and the trusted first-party client preload. */

export const APP_ID = 'one.leaper.vibeface.desktop'
export const PRODUCT_NAME = 'Vibeface'

/** Hidden Core flag: the owned child shuts down gracefully when the desktop closes its stdin. */
export const PARENT_STDIN_FLAG = '--desktop-parent-stdin'

export const DESKTOP_NODE_SETTINGS_PATH = '/settings/desktop-node'

export const IPC = {
  status: 'desktop:status',
  getStatus: 'desktop:get-status',
  chooseDataDir: 'desktop:choose-data-dir',
  useDataDir: 'desktop:use-data-dir',
  forgetDataDir: 'desktop:forget-data-dir',
  startOwned: 'desktop:start-owned',
  attachExisting: 'desktop:attach-existing',
  stopOwned: 'desktop:stop-owned',
  focusClient: 'desktop:focus-client',
  reconnectClient: 'desktop:reconnect-client',
  setPreference: 'desktop:set-preference',
  openExternal: 'desktop:open-external',
  discoverNodes: 'desktop:discover-nodes',
  connectDiscoveredNode: 'desktop:connect-discovered-node',
  startDiscoveredNode: 'desktop:start-discovered-node',
  installBackgroundNode: 'desktop:install-background-node',
  startBackgroundNode: 'desktop:start-background-node',
  stopBackgroundNode: 'desktop:stop-background-node',
  removeBackgroundNode: 'desktop:remove-background-node',
  updateBackgroundNode: 'desktop:update-background-node',
  chooseNodeMode: 'desktop:choose-node-mode',
} as const

export type BackgroundRuntimeUpdateAvailability =
  | 'not_installed'
  | 'current'
  | 'newer_available'
  | 'immutable_conflict'
  | 'downgrade_refused'

export type BackgroundRuntimeUpdate = {
  bundledVersion: string
  installedVersion: string | null
  runningVersion: string | null
  availability: BackgroundRuntimeUpdateAvailability
}

/** Read-only current-user Node observations; kept separate from App-host lifecycle state. */
export type NodeSource = 'remembered' | 'app_profile' | 'default_home' | 'user_service' | 'running_process'
type NodeIdentity = { id: string; deviceId: string; dataDir: string; sources: NodeSource[] }
type AvailableNode = NodeIdentity & {
  localListen: string
  listen: string
  webEnabled: boolean
  coreVersion: string
}
export type LocalNode =
  | (AvailableNode & { kind: 'running' })
  | (AvailableNode & { kind: 'stopped' })
  | (NodeIdentity & {
      kind: 'unavailable'
      reason: 'identity_mismatch' | 'maintenance_unavailable' | 'inspection_unavailable'
    })

export type NodeDiscovery =
  | { kind: 'idle' | 'scanning' }
  | { kind: 'ready'; nodes: LocalNode[]; warnings: string[] }
  | { kind: 'failed'; message: string }

export type BackgroundNode =
  | { kind: 'absent' | 'checking'; runtimeUpdate?: BackgroundRuntimeUpdate }
  | { kind: 'installed'; nodeId: string; dataDir: string; health: 'running' | 'stopped' | 'unhealthy'; manager: 'launchd' | 'systemd' | 'task-scheduler'; runtimeUpdate: BackgroundRuntimeUpdate }
  | { kind: 'partial'; nodeId: string; dataDir: string; message: string; runtimeUpdate?: BackgroundRuntimeUpdate }
  | { kind: 'conflict'; message: string; runtimeUpdate?: BackgroundRuntimeUpdate }
  | { kind: 'failure'; message: string; runtimeUpdate?: BackgroundRuntimeUpdate }

/** Core availability as the desktop can actually prove it, not a guess. */
export type RuntimeState =
  | { kind: 'ready'; corePath: string }
  | { kind: 'missing'; corePath: string; detail: string }

/** Who owns the Core process behind the current data directory. */
export type CoreState =
  /** No data directory selected yet. */
  | { kind: 'none' }
  /** Selected directory holds no Core data; starting would initialize it. */
  | { kind: 'uninitialized'; dataDir: string }
  /** Initialized and not running: offline until the user starts or another service appears. */
  | { kind: 'offline'; dataDir: string; deviceId: string | null }
  /** This app spawned a child and is waiting for its control health to report running. */
  | { kind: 'starting'; dataDir: string; attempts: number }
  /** A Core this app owns; quitting stops it. */
  | { kind: 'owned'; dataDir: string; deviceId: string; origin: string }
  /** A Core this app did not start; quitting leaves it running. */
  | { kind: 'attached'; dataDir: string; deviceId: string; origin: string }
  /** The last operation failed; the message is Core's own, without tokens. */
  | { kind: 'failed'; dataDir: string | null; message: string }

export type Preferences = {
  /** Open the Vibeface window as soon as a Core is reachable. */
  openBusinessOnConnect: boolean
}

export const PREFERENCE_KEYS = ['openBusinessOnConnect'] as const
export type PreferenceKey = (typeof PREFERENCE_KEYS)[number]

export type DesktopStatus = {
  runtime: RuntimeState
  core: CoreState
  /** Host startup has finished navigation and may accept native mutations. */
  hostPhase: 'starting' | 'ready'
  /** True while the sole client window is visible. */
  clientVisible: boolean
  preferences: Preferences
  /** Absolute profile directory holding desktop preferences and the Chromium profile. */
  profileDir: string
  version: string
  /** Connection-only session: the desktop will not start or stop any Core. */
  attachOnly: boolean
  /** Desktop client is usable independently of whether the bundled Core runtime is available. */
  clientAvailable: boolean
  /** User's first-run decision and whether this App may host an optional local Node. */
  nodeMode: 'undecided' | 'controller' | 'local'
  /** Last verified client target, independently of the App's Core child. */
  selectedNode: Pick<LocalNode, 'deviceId' | 'dataDir'> | null
  discovery: NodeDiscovery
  backgroundNode: BackgroundNode
}

export type DesktopNodeApi = {
  getStatus(): Promise<DesktopStatus>
  chooseDataDir(): Promise<DesktopStatus>
  useDataDir(dataDir: string): Promise<DesktopStatus>
  forgetDataDir(): Promise<DesktopStatus>
  startOwned(): Promise<DesktopStatus>
  attachExisting(): Promise<DesktopStatus>
  stopOwned(): Promise<DesktopStatus>
  focusClient(): Promise<DesktopStatus>
  reconnectClient(showChat: boolean): void
  setPreference(key: PreferenceKey, value: boolean): Promise<DesktopStatus>
  openExternal(url: string): Promise<void>
  onStatus(handler: (status: DesktopStatus) => void): () => void
  discoverNodes(): Promise<DesktopStatus>
  connectDiscoveredNode(candidateId: string): Promise<DesktopStatus>
  startDiscoveredNode(candidateId: string): Promise<DesktopStatus>
  chooseNodeMode(mode: 'controller' | 'local'): Promise<DesktopStatus>
  installBackgroundNode(): Promise<DesktopStatus>
  startBackgroundNode(): Promise<DesktopStatus>
  stopBackgroundNode(): Promise<DesktopStatus>
  removeBackgroundNode(): Promise<DesktopStatus>
  updateBackgroundNode(): Promise<DesktopStatus>
}
