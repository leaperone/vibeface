import { spawn } from 'node:child_process'
import { once } from 'node:events'
import type { Readable } from 'node:stream'

const MAX_RESULT_BYTES = 64 * 1024
const COMMAND_TIMEOUT_MS = 30_000

export type ManagedNodeProgram = { command: string; args?: readonly string[] }
type CoreStatus = 'running' | 'stopped' | 'unavailable' | null
type ManagedNodeIdentity = { device_id: string; data_dir: string; service_id: string; managed_root: string }
export type ManagedNodeStatus =
  | { ok: true; state: 'not_installed'; device_id: null; data_dir: null; service_id: null; managed_root: null; service_running: null; core_status: null }
  | ({ ok: true; state: 'installed' | 'installed_stopped' | 'partial'; service_running: boolean; core_status: CoreStatus } & ManagedNodeIdentity)
  | ({ ok: true; state: 'conflict' | 'unavailable'; service_running: boolean | null; core_status: CoreStatus } & ManagedNodeIdentity)

type Options = {
  profileDir: string
  bundledCli: ManagedNodeProgram
  onChange?: (status: ManagedNodeStatus) => void
}

async function readBounded(stream: Readable, limit: number): Promise<string> {
  const chunks: Buffer[] = []
  let size = 0
  for await (const chunk of stream) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), 'utf8')
    size += bytes.length
    if (size > limit) throw new Error('desktop Node result exceeds its size limit')
    chunks.push(bytes)
  }
  return Buffer.concat(chunks, size).toString('utf8')
}

function isStatus(value: unknown): value is ManagedNodeStatus {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false
  if (!('ok' in value) || !('state' in value) || !('device_id' in value) || !('data_dir' in value) ||
      !('service_id' in value) || !('managed_root' in value) || !('service_running' in value) || !('core_status' in value)) return false
  if (Object.keys(value).length !== 8 || value.ok !== true) return false
  if (value.state === 'not_installed') {
    return value.device_id === null && value.data_dir === null && value.service_id === null &&
      value.managed_root === null && value.service_running === null && value.core_status === null
  }
  const identityValid = typeof value.device_id === 'string' && typeof value.data_dir === 'string' &&
    typeof value.service_id === 'string' && typeof value.managed_root === 'string'
  const coreStatusValid = value.core_status === null || value.core_status === 'running' ||
    value.core_status === 'stopped' || value.core_status === 'unavailable'
  if (!identityValid || !coreStatusValid) return false
  switch (value.state) {
    case 'installed':
    case 'installed_stopped':
    case 'partial':
      return typeof value.service_running === 'boolean'
    case 'conflict':
    case 'unavailable':
      return value.service_running === null || typeof value.service_running === 'boolean'
    default:
      return false
  }
}

async function invoke(program: ManagedNodeProgram, profileDir: string, action: string, expectedDeviceId?: string): Promise<ManagedNodeStatus> {
  const args = [...(program.args ?? []), 'desktop-node', '--profile-dir', profileDir]
  if (expectedDeviceId !== undefined) args.push('--expected-device-id', expectedDeviceId)
  args.push(action)
  const child = spawn(program.command, args, { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  const { stdout, stderr } = child
  if (!stdout || !stderr) throw new Error('desktop Node command streams are unavailable')
  const outputPromise = readBounded(stdout, MAX_RESULT_BYTES)
  const errorDrain = readBounded(stderr, MAX_RESULT_BYTES).catch(() => '')
  void errorDrain.catch(() => undefined)
  const closePromise = once(child, 'close').then(([code, signal]: unknown[]) => ({
    code: typeof code === 'number' || code === null ? code : null,
    signal: typeof signal === 'string' || signal === null ? signal : null,
  }))
  const timeout = new Promise<never>((_, reject) => {
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      reject(new Error('desktop Node command timed out'))
    }, COMMAND_TIMEOUT_MS)
    void closePromise.finally(() => clearTimeout(timer))
  })
  try {
    const [text, close] = await Promise.race([Promise.all([outputPromise, closePromise]), timeout])
    if (close.code !== 0 || close.signal !== null) throw new Error('desktop Node command failed')
    const lines = text.trim().split(/\r?\n/)
    if (lines.length !== 1 || !lines[0]) throw new Error('desktop Node returned an invalid result')
    let value: unknown
    try { value = JSON.parse(lines[0]) } catch { throw new Error('desktop Node returned invalid JSON') }
    if (!isStatus(value)) throw new Error('desktop Node returned an unsupported result')
    return value
  } catch {
    try { child.kill('SIGKILL') } catch { /* already closed */ }
    throw new Error('managed Node operation failed')
  }
}

/** Calls only the hidden, fixed desktop-node CLI commands with argv; no shell or generic process API. */
export class ManagedNodeController {
  readonly #profileDir: string
  readonly #bundledCli: ManagedNodeProgram
  readonly #onChange?: (status: ManagedNodeStatus) => void

  constructor(options: Options) {
    this.#profileDir = options.profileDir
    this.#bundledCli = options.bundledCli
    this.#onChange = options.onChange
  }

  async status(): Promise<ManagedNodeStatus> {
    return this.#run('status')
  }

  async installBackground(expectedNodeId: string): Promise<ManagedNodeStatus> {
    return this.#run('install', expectedNodeId)
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
    const result = await invoke(this.#bundledCli, this.#profileDir, action, expectedNodeId)
    this.#onChange?.(result)
    return result
  }
}
