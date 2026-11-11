import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { readBounded } from './bounded-read.js'

const MAX_RESULT_BYTES = 64 * 1024
const STATUS_COMMAND_TIMEOUT_MS = 30_000
/** Covers lock wait, health, and acknowledgement. A timeout is an unknown outcome: the controller does not write or restart. */
const UPDATE_COMMAND_TIMEOUT_MS = 120_000

export function commandTimeoutMs(action: string): number {
  return action === 'update' ? UPDATE_COMMAND_TIMEOUT_MS : STATUS_COMMAND_TIMEOUT_MS
}

const CLI_FAILURES: Record<string, string> = {
  desktop_node_package_invalid: '安装包校验未通过，请重新安装桌面 App。',
  desktop_node_service_failed: '系统服务操作没有成功，请重新核对后台节点状态。',
  desktop_node_health_unconfirmed: '后台节点尚未确认就绪，请重新核对状态。',
  desktop_node_lock_unconfirmed: '无法确认节点是否已退出，请重新核对状态。',
  desktop_node_registration_unconfirmed: '无法确认系统中的节点服务，请重新核对状态。',
  desktop_node_update_refused: '当前无法更新本机后台节点，请先核对运行状态。',
  desktop_node_recovery_required: '上次更新尚未完成，请在后台节点停止后点击“继续更新”核对。',
}
class CliOperationFailure extends Error {}

export type ManagedNodeProgram = { command: string; args?: readonly string[] }
type CoreStatus = 'running' | 'stopped' | 'unavailable' | null

export type RuntimeUpdateAvailability =
  | 'not_installed'
  | 'current'
  | 'newer_available'
  | 'immutable_conflict'
  | 'downgrade_refused'

export type ManagedNodeRuntimeUpdate = {
  bundledVersion: string
  installedVersion: string | null
  runningVersion: string | null
  availability: RuntimeUpdateAvailability
  recoveryRequired: boolean
}

type ManagedNodeIdentity = { device_id: string; data_dir: string; service_id: string; managed_root: string }
export type ManagedNodeStatus =
  | { ok: true; state: 'not_installed'; device_id: null; data_dir: null; service_id: null; managed_root: null; service_running: null; core_status: null; runtime_update: ManagedNodeRuntimeUpdate }
  | ({ ok: true; state: 'installed' | 'installed_stopped' | 'partial'; service_running: boolean; core_status: CoreStatus; runtime_update: ManagedNodeRuntimeUpdate } & ManagedNodeIdentity)
  | ({ ok: true; state: 'conflict' | 'unavailable'; service_running: boolean | null; core_status: CoreStatus; runtime_update: ManagedNodeRuntimeUpdate } & ManagedNodeIdentity)

type Options = {
  profileDir: string
  bundledCli: ManagedNodeProgram
  onChange?: (status: ManagedNodeStatus) => void
  commandTimeoutMs?: (action: string) => number
}

const RESULT_OVERFLOW = 'desktop Node result exceeds its size limit'

function runtimeUpdateAvailability(value: unknown): RuntimeUpdateAvailability {
  if (value === 'not_installed' || value === 'current' || value === 'newer_available' || value === 'immutable_conflict' || value === 'downgrade_refused') {
    return value
  }
  throw new Error('desktop Node returned unsupported runtime update availability')
}

function runtimeUpdate(value: unknown): ManagedNodeRuntimeUpdate {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('desktop Node returned invalid runtime update metadata')
  }
  const source = value as Record<string, unknown>
  if (!('bundled_version' in source) || !('availability' in source)) {
    throw new Error('desktop Node returned invalid runtime update metadata')
  }
  if (typeof source.bundled_version !== 'string' || !source.bundled_version) {
    throw new Error('desktop Node returned invalid bundled runtime version')
  }
  const installedVersion = source.installed_version
  if (!(installedVersion === null || (typeof installedVersion === 'string' && installedVersion))) {
    throw new Error('desktop Node returned invalid installed runtime version')
  }
  const runningVersion = source.running_version
  if (!(runningVersion === null || (typeof runningVersion === 'string' && runningVersion))) {
    throw new Error('desktop Node returned invalid running runtime version')
  }
  if (typeof source.recovery_required !== 'boolean') {
    throw new Error('desktop Node returned invalid recovery state')
  }
  return {
    bundledVersion: source.bundled_version,
    installedVersion,
    runningVersion,
    availability: runtimeUpdateAvailability(source.availability),
    recoveryRequired: source.recovery_required,
  }
}

export function parseManagedNodeStatus(value: unknown): ManagedNodeStatus {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('desktop Node returned an unsupported result')
  }
  const raw = value as Record<string, unknown>
  if (Object.keys(raw).length !== 9 || raw.ok !== true || typeof raw.state !== 'string') {
    throw new Error('desktop Node returned an unsupported result')
  }
  const runtime = runtimeUpdate(raw.runtime_update)
  const state = raw.state
  if (state === 'not_installed') {
    if (raw.device_id !== null || raw.data_dir !== null || raw.service_id !== null || raw.managed_root !== null ||
        raw.service_running !== null || raw.core_status !== null || runtime.availability !== 'not_installed' || runtime.recoveryRequired) {
      throw new Error('desktop Node returned an unsupported result')
    }
    return {
      ok: true,
      state,
      device_id: null,
      data_dir: null,
      service_id: null,
      managed_root: null,
      service_running: null,
      core_status: null,
      runtime_update: runtime,
    }
  }
  if (typeof raw.device_id !== 'string' || typeof raw.data_dir !== 'string' || typeof raw.service_id !== 'string' ||
      typeof raw.managed_root !== 'string') {
    throw new Error('desktop Node returned an unsupported result')
  }
  let coreStatus: CoreStatus
  if (raw.core_status === null || raw.core_status === 'running' || raw.core_status === 'stopped' || raw.core_status === 'unavailable') {
    coreStatus = raw.core_status
  } else {
    throw new Error('desktop Node returned an unsupported result')
  }
  const serviceRunning = raw.service_running
  const identity = {
    device_id: raw.device_id,
    data_dir: raw.data_dir,
    service_id: raw.service_id,
    managed_root: raw.managed_root,
    core_status: coreStatus,
    runtime_update: runtime,
  }
  if (state === 'installed' || state === 'installed_stopped' || state === 'partial') {
    if (typeof serviceRunning !== 'boolean') throw new Error('desktop Node returned an unsupported result')
    return { ok: true, state, service_running: serviceRunning, ...identity }
  }
  if (state === 'conflict' || state === 'unavailable') {
    if (!(serviceRunning === null || typeof serviceRunning === 'boolean')) {
      throw new Error('desktop Node returned an unsupported result')
    }
    return { ok: true, state, service_running: serviceRunning, ...identity }
  }
  throw new Error('desktop Node returned an unsupported result')
}

async function invoke(program: ManagedNodeProgram, args: readonly string[], timeoutMs: number): Promise<unknown> {
  const child = spawn(program.command, [...(program.args ?? []), ...args], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  const { stdout, stderr } = child
  if (!stdout || !stderr) throw new Error('desktop Node command streams are unavailable')
  const outputPromise = readBounded(stdout, MAX_RESULT_BYTES, RESULT_OVERFLOW)
  const errorDrain = readBounded(stderr, MAX_RESULT_BYTES, RESULT_OVERFLOW).catch(() => '')
  void errorDrain.catch(() => undefined)
  const closePromise = once(child, 'close').then(([code, signal]: unknown[]) => ({
    code: typeof code === 'number' || code === null ? code : null,
    signal: typeof signal === 'string' || signal === null ? signal : null,
  }))
  const timeout = new Promise<never>((_, reject) => {
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      reject(new Error('desktop Node command timed out'))
    }, timeoutMs)
    void closePromise.then(() => clearTimeout(timer), () => clearTimeout(timer))
  })
  try {
    const [text, close] = await Promise.race([Promise.all([outputPromise, closePromise]), timeout])
    if (close.code !== 0 || close.signal !== null) {
      const diagnostic = await errorDrain
      const code = /^Error: (desktop_node_[a-z_]+)\s*$/m.exec(diagnostic)?.[1]
      const message = code ? CLI_FAILURES[code] : undefined
      throw new CliOperationFailure(message ?? 'managed Node operation failed')
    }
    const lines = text.trim().split(/\r?\n/)
    if (lines.length !== 1 || !lines[0]) throw new Error('desktop Node returned an invalid result')
    let value: unknown
    try { value = JSON.parse(lines[0]) } catch { throw new Error('desktop Node returned invalid JSON') }
    return value
  } catch (error) {
    try { child.kill('SIGKILL') } catch { /* already closed */ }
    if (error instanceof CliOperationFailure) throw error
    throw new Error('managed Node operation failed')
  }
}

/** Calls only the hidden, fixed desktop-node CLI commands with argv; no shell or generic process API. */
export class ManagedNodeController {
  readonly #profileDir: string
  readonly #bundledCli: ManagedNodeProgram
  readonly #onChange?: (status: ManagedNodeStatus) => void
  readonly #commandTimeoutMs: (action: string) => number

  constructor(options: Options) {
    this.#profileDir = options.profileDir
    this.#bundledCli = options.bundledCli
    this.#onChange = options.onChange
    this.#commandTimeoutMs = options.commandTimeoutMs ?? commandTimeoutMs
  }

  async prepareProfile(): Promise<void> {
    const result = await invoke(this.#bundledCli, ['desktop-profile', '--profile-dir', this.#profileDir], this.#commandTimeoutMs('profile'))
    if (typeof result !== 'object' || result === null || Array.isArray(result) || Object.keys(result).length !== 1 || !('ok' in result) || result.ok !== true) {
      throw new Error('Desktop profile permissions could not be prepared')
    }
  }

  async status(): Promise<ManagedNodeStatus> {
    return this.#run('status')
  }

  async installBackground(expectedNodeId: string): Promise<ManagedNodeStatus> {
    return this.#run('install', expectedNodeId)
  }

  async updateBackground(expectedNodeId: string): Promise<ManagedNodeStatus> {
    return this.#run('update', expectedNodeId)
  }

  async start(expectedNodeId: string): Promise<ManagedNodeStatus> {
    return this.#run('start', expectedNodeId)
  }

  async stop(expectedNodeId: string): Promise<ManagedNodeStatus> {
    return this.#run('stop', expectedNodeId)
  }

  async uninstall(expectedNodeId: string): Promise<ManagedNodeStatus> {
    return this.#run('uninstall', expectedNodeId)
  }

  async #run(action: string, expectedNodeId?: string): Promise<ManagedNodeStatus> {
    const args = ['desktop-node', '--profile-dir', this.#profileDir]
    if (expectedNodeId !== undefined) args.push('--expected-device-id', expectedNodeId)
    args.push(action)
    const result = await invoke(this.#bundledCli, args, this.#commandTimeoutMs(action))
    const status = parseManagedNodeStatus(result)
    this.#onChange?.(status)
    return status
  }
}
