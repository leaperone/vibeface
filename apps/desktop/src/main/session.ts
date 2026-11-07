import { existsSync } from 'node:fs'
import { isAbsolute, join } from 'node:path'
import { runControl, type CoreProgram } from './control-client.js'
import { parseAccess, parseInspect, type Inspect } from './control-types.js'
import { businessUrl, parsePrivateOrigin } from './business-url.js'
import { CoreLifecycle } from './lifecycle.js'
import { defaultSettings, loadSettings, saveSettings, type Settings } from './settings.js'
import type { CoreState, PreferenceKey, RuntimeState } from '../shared/contract.js'

/** The exact programs this build may run; injected so tests drive real child processes. */
export type Programs = {
  runtime(): RuntimeState
  control(): CoreProgram
  service(dataDir: string): CoreProgram
}

export type SessionOptions = {
  profileDir: string
  /** Connection-only run: this session never starts, initializes or stops a Core. */
  attachOnly: boolean
  onChange(): void
  /** Overridable so tests do not wait 30 seconds for a failing start. */
  startTimeoutMs?: number
}

const START_TIMEOUT_MS = 30_000
const START_POLL_MS = 250

function message(error: unknown): string {
  return error instanceof Error ? error.message : '出现未知错误。'
}

/** Core data exists here when it holds both private files `init` creates. */
function initialized(dataDir: string): boolean {
  return existsSync(join(dataDir, 'config.json')) && existsSync(join(dataDir, 'identity.json'))
}

export class CoreSession {
  private readonly lifecycle = new CoreLifecycle()
  private settings: Settings = defaultSettings()
  private core: CoreState = { kind: 'none' }
  private busy = false

  constructor(
    private readonly programs: Programs,
    private readonly options: SessionOptions
  ) {
    this.lifecycle.onOwnedExit((exit) => {
      if (this.core.kind !== 'owned' && this.core.kind !== 'starting') return
      const dataDir = this.lifecycleDataDir()
      this.core =
        exit.code === 0 || exit.code === null
          ? { kind: 'offline', dataDir: dataDir ?? this.settings.lastDataDir ?? '', deviceId: null }
          : { kind: 'failed', dataDir, message: `本机 Core 已退出（代码 ${exit.code}）。` }
      if (this.core.kind === 'offline' && !this.core.dataDir) this.core = { kind: 'none' }
      this.options.onChange()
    })
  }

  state(): CoreState {
    return this.core
  }

  preferences(): Settings['preferences'] {
    return this.settings.preferences
  }

  dataDir(): string | null {
    return this.lifecycleDataDir() ?? this.settings.lastDataDir
  }

  private lifecycleDataDir(): string | null {
    const current = this.core
    return current.kind === 'none' || current.kind === 'failed' ? null : current.dataDir
  }

  /** Reopen the last directory read-only: nothing is started and nothing is written. */
  async restore(): Promise<void> {
    this.settings = await loadSettings(this.options.profileDir)
    const dataDir = this.settings.lastDataDir
    if (!dataDir) return
    await this.refresh(dataDir)
  }

  async setPreference(key: PreferenceKey, value: boolean): Promise<void> {
    this.settings = { ...this.settings, preferences: { ...this.settings.preferences, [key]: value } }
    await saveSettings(this.options.profileDir, this.settings)
    this.options.onChange()
  }

  async forgetDataDir(): Promise<void> {
    if (this.lifecycle.state().kind === 'owned') {
      throw new Error('先停止这台电脑上由 Vibeface 启动的 Core，再切换数据目录。')
    }
    this.lifecycle.detach()
    this.settings = { ...this.settings, lastDataDir: null }
    await saveSettings(this.options.profileDir, this.settings)
    this.core = { kind: 'none' }
    this.options.onChange()
  }

  async useDataDir(dataDir: string): Promise<void> {
    if (!isAbsolute(dataDir)) throw new Error('请选择一个绝对路径的数据目录。')
    if (this.lifecycle.state().kind === 'owned' && this.lifecycle.dataDir() !== dataDir) {
      throw new Error('先停止这台电脑上由 Vibeface 启动的 Core，再切换数据目录。')
    }
    this.settings = { ...this.settings, lastDataDir: dataDir }
    await saveSettings(this.options.profileDir, this.settings)
    await this.refresh(dataDir)
  }

  /** Read Core's own view of the directory; never writes and never takes ownership. */
  async refresh(dataDir: string): Promise<void> {
    if (this.programs.runtime().kind === 'missing') {
      this.core = { kind: 'failed', dataDir, message: '找不到捆绑的 Core 程序。' }
      this.options.onChange()
      return
    }
    if (!initialized(dataDir)) {
      this.core = { kind: 'uninitialized', dataDir }
      this.options.onChange()
      return
    }
    try {
      const inspect = await this.inspect(dataDir)
      this.core = this.stateFromInspect(inspect)
    } catch (error) {
      this.core = { kind: 'failed', dataDir, message: message(error) }
    }
    this.options.onChange()
  }

  private stateFromInspect(inspect: Inspect): CoreState {
    const owned = this.lifecycle.state().kind === 'owned' && this.lifecycle.dataDir() === inspect.dataDir
    if (inspect.status !== 'running') {
      if (owned) return { kind: 'starting', dataDir: inspect.dataDir, attempts: 0 }
      return { kind: 'offline', dataDir: inspect.dataDir, deviceId: inspect.deviceId }
    }
    const origin = inspect.publicEntry ? '' : parsePrivateOrigin(inspect.localListen).origin
    return {
      kind: owned ? 'owned' : 'attached',
      dataDir: inspect.dataDir,
      deviceId: inspect.deviceId,
      origin,
    }
  }

  private async inspect(dataDir: string): Promise<Inspect> {
    return parseInspect(await runControl(this.programs.control(), { command: 'inspect', data_dir: dataDir }))
  }

  /** Connection only: an already running service keeps its own owner. */
  async attachExisting(): Promise<void> {
    await this.guard(async () => {
      const dataDir = this.requireDataDir()
      const inspect = await this.inspect(dataDir)
      if (inspect.status !== 'running') {
        throw new Error('这个数据目录上没有正在运行的 Core，无法仅连接。')
      }
      this.lifecycle.attach(dataDir)
      this.core = this.stateFromInspect(inspect)
    })
  }

  /**
   * Start a Core this app owns. An empty or missing directory is initialized through Core's
   * own `init` first; a directory another process already serves is never taken over.
   */
  async startOwned(): Promise<void> {
    await this.guard(async () => {
      if (this.options.attachOnly) throw new Error('这次会话以仅连接模式启动，不会启动 Core。')
      if (!this.dataDir()) {
        await this.useDataDir(join(this.options.profileDir, 'node'))
      }
      const dataDir = this.requireDataDir()
      if (this.lifecycle.state().kind === 'owned') throw new Error('这台电脑上的 Core 已经在运行。')
      if (initialized(dataDir)) {
        const inspect = await this.inspect(dataDir)
        if (inspect.status === 'running') {
          throw new Error('这个数据目录已经有 Core 在运行，请改用仅连接。')
        }
      } else {
        await runControl(this.programs.control(), { command: 'init', options: { data_dir: dataDir } })
      }
      this.core = { kind: 'starting', dataDir, attempts: 0 }
      this.options.onChange()
      this.lifecycle.startOwned(this.programs.service(dataDir), dataDir)
      await this.awaitRunning(dataDir)
    })
  }

  private async awaitRunning(dataDir: string): Promise<void> {
    const deadline = Date.now() + (this.options.startTimeoutMs ?? START_TIMEOUT_MS)
    let attempts = 0
    let last = '本机 Core 没有在预期时间内就绪。'
    while (Date.now() < deadline) {
      if (this.lifecycle.state().kind !== 'owned') {
        throw new Error('本机 Core 在启动过程中退出。')
      }
      attempts += 1
      this.core = { kind: 'starting', dataDir, attempts }
      this.options.onChange()
      try {
        const inspect = await this.inspect(dataDir)
        if (inspect.status === 'running') {
          this.core = this.stateFromInspect(inspect)
          return
        }
        last = inspect.error ?? last
      } catch (error) {
        last = message(error)
      }
      await new Promise((resolve) => setTimeout(resolve, START_POLL_MS))
    }
    await this.lifecycle.stopOwned('replace')
    throw new Error(last)
  }

  async stopOwned(): Promise<void> {
    await this.guard(async () => {
      if (this.lifecycle.state().kind !== 'owned') {
        throw new Error('这台电脑上没有由 Vibeface 启动的 Core。')
      }
      await this.lifecycle.stopOwned('quit')
      const dataDir = this.settings.lastDataDir
      if (dataDir) await this.refresh(dataDir)
    })
  }

  /** Resolve the Core-served URL for the business window, canonicalized to Core's private origin. */
  async businessTarget(): Promise<string> {
    const dataDir = this.requireDataDir()
    const inspect = await this.inspect(dataDir)
    if (inspect.status !== 'running') throw new Error('Core 还没有运行，无法打开 Vibeface 窗口。')
    if (!inspect.webEnabled) throw new Error('这个 Core 关闭了网页访问，请在配置中重新开启。')
    const access = parseAccess(
      await runControl(this.programs.control(), { command: 'access', data_dir: dataDir, json: true })
    )
    this.core = this.stateFromInspect(inspect)
    this.options.onChange()
    return businessUrl(access, inspect)
  }

  /** Quit closes only a Core this app started; an attached service is left alone. */
  async shutdown(): Promise<void> {
    await this.lifecycle.stopOwned('quit')
  }

  ownsCore(): boolean {
    return this.lifecycle.state().kind === 'owned'
  }

  private requireDataDir(): string {
    const dataDir = this.dataDir()
    if (!dataDir) throw new Error('请先选择 Core 数据目录。')
    return dataDir
  }

  private async guard(work: () => Promise<void>): Promise<void> {
    if (this.busy) throw new Error('上一个操作还在进行，请稍候。')
    this.busy = true
    try {
      await work()
    } catch (error) {
      this.core = { kind: 'failed', dataDir: this.dataDir(), message: message(error) }
      throw error
    } finally {
      this.busy = false
      this.options.onChange()
    }
  }
}
