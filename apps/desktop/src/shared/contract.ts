/** Types shared by the main process, the trusted launcher preload and the launcher renderer. */

export const LAUNCHER_SCHEME = 'vibeface-launcher'
export const LAUNCHER_HOST = 'app'
export const LAUNCHER_ORIGIN = `${LAUNCHER_SCHEME}://${LAUNCHER_HOST}`
export const LAUNCHER_URL = `${LAUNCHER_ORIGIN}/index.html`

export const APP_ID = 'one.leaper.vibeface.desktop'
export const PRODUCT_NAME = 'Vibeface'

/** Hidden Core flag: the owned child shuts down gracefully when the desktop closes its stdin. */
export const PARENT_STDIN_FLAG = '--desktop-parent-stdin'

export const IPC = {
  status: 'launcher:status',
  getStatus: 'launcher:get-status',
  chooseDataDir: 'launcher:choose-data-dir',
  useDataDir: 'launcher:use-data-dir',
  forgetDataDir: 'launcher:forget-data-dir',
  startOwned: 'launcher:start-owned',
  attachExisting: 'launcher:attach-existing',
  stopOwned: 'launcher:stop-owned',
  openBusiness: 'launcher:open-business',
  setPreference: 'launcher:set-preference',
  openExternal: 'launcher:open-external',
  discoverNodes: 'launcher:discover-nodes',
  connectDiscoveredNode: 'launcher:connect-discovered-node',
  startDiscoveredNode: 'launcher:start-discovered-node',
  installBackgroundNode: 'launcher:install-background-node',
  startBackgroundNode: 'launcher:start-background-node',
  stopBackgroundNode: 'launcher:stop-background-node',
  removeBackgroundNode: 'launcher:remove-background-node',
  chooseNodeMode: 'launcher:choose-node-mode',
} as const

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
  | { kind: 'absent' | 'checking' }
  | { kind: 'installed'; nodeId: string; dataDir: string; health: 'running' | 'stopped' | 'unhealthy'; manager: 'launchd' | 'systemd' | 'task-scheduler' }
  | { kind: 'partial'; message: string }
  | { kind: 'conflict'; message: string }
  | { kind: 'failure'; message: string }

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

export type LauncherStatus = {
  runtime: RuntimeState
  core: CoreState
  /** True while the business window exists and shows the Core-served UI. */
  businessOpen: boolean
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

export type LauncherApi = {
  getStatus(): Promise<LauncherStatus>
  chooseDataDir(): Promise<LauncherStatus>
  useDataDir(dataDir: string): Promise<LauncherStatus>
  forgetDataDir(): Promise<LauncherStatus>
  startOwned(): Promise<LauncherStatus>
  attachExisting(): Promise<LauncherStatus>
  stopOwned(): Promise<LauncherStatus>
  openBusiness(): Promise<LauncherStatus>
  setPreference(key: PreferenceKey, value: boolean): Promise<LauncherStatus>
  openExternal(url: string): Promise<void>
  onStatus(handler: (status: LauncherStatus) => void): () => void
  discoverNodes(): Promise<LauncherStatus>
  connectDiscoveredNode(candidateId: string): Promise<LauncherStatus>
  startDiscoveredNode(candidateId: string): Promise<LauncherStatus>
  chooseNodeMode(mode: 'controller' | 'local'): Promise<LauncherStatus>
  installBackgroundNode(): Promise<LauncherStatus>
  startBackgroundNode(): Promise<LauncherStatus>
  stopBackgroundNode(): Promise<LauncherStatus>
  removeBackgroundNode(): Promise<LauncherStatus>
}
