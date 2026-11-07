import { lstat, readdir, realpath, stat } from 'node:fs/promises'
import { isAbsolute, join } from 'node:path'
import { parseInspect, type Access, type Inspect } from './control-types.js'
import { runControl } from './control-client.js'
import { discoverLocalNodes, type LocalNodes } from './local-nodes.js'
import type { ManagedNodeController, ManagedNodeStatus } from './managed-node.js'
import { loadClientPreferences, saveClientPreferences, type ClientPreferences } from './client-preferences.js'
import type { CoreProgram } from './control-client.js'
import type { BackgroundNode, LauncherStatus, LocalNode, NodeDiscovery } from '../shared/contract.js'

export type SelectedLocalNode = { inspect: Inspect; access: Access }
export type DesktopCoordinatorOptions = {
  profileDir: string
  controlProgram: CoreProgram
  discover?: (program: CoreProgram, profileDir: string, remembered: string[]) => Promise<LocalNodes>
  inspect(dataDir: string): Promise<Inspect>
  access(dataDir: string): Promise<Access>
  managedNode: ManagedNodeController
  startDiscovered(node: LocalNode): Promise<void>
  startOwnedAppNode(): Promise<void>
  prepareBackgroundNode(): Promise<Inspect>
  beforeInstallBackground?(node: Inspect): Promise<void>
  onChange?(): void
  platform?: NodeJS.Platform
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : '操作失败。'
}

/** Rust and Node may spell a Windows canonical path with different namespace prefixes. */
async function sameDirectory(first: string, second: string): Promise<boolean> {
  const [a, b] = await Promise.all([stat(first, { bigint: true }), stat(second, { bigint: true })])
  return a.isDirectory() && b.isDirectory() && a.dev === b.dev && a.ino === b.ino
}

function manager(platform: NodeJS.Platform): 'launchd' | 'systemd' | 'task-scheduler' {
  if (platform === 'darwin') return 'launchd'
  if (platform === 'win32') return 'task-scheduler'
  return 'systemd'
}

export function backgroundNodeFromStatus(status: ManagedNodeStatus, platform: NodeJS.Platform): BackgroundNode {
  if (!status.ok) return { kind: 'failure', message: '无法读取后台节点状态。' }
  if (status.state === 'not_installed') return { kind: 'absent' }
  if (status.state === 'conflict') return { kind: 'conflict', message: '后台节点服务注册与 Vibeface 记录不一致。' }
  if (status.state === 'unavailable') return { kind: 'failure', message: '无法检查后台节点服务健康状态。' }
  if (status.state === 'partial') return { kind: 'partial', message: '后台节点安装未完整，请先检查服务状态。' }
  const health = status.service_running === true && status.core_status === 'running'
    ? 'running'
    : status.service_running === false && status.core_status === 'stopped' ? 'stopped' : 'unhealthy'
  return { kind: 'installed', nodeId: status.device_id, dataDir: status.data_dir, health, manager: manager(platform) }
}

/** Prepare only the App profile Node, and only when the user explicitly asks for background install. */
export async function prepareAppProfileNode(
  profileDir: string,
  program: CoreProgram,
  run: typeof runControl = runControl,
): Promise<Inspect> {
  const dataDir = join(profileDir, 'node')
  let entries: string[]
  try {
    const metadata = await lstat(dataDir)
    if (metadata.isSymbolicLink() || !metadata.isDirectory()) throw new Error('应用节点目录不是普通目录。')
    entries = await readdir(dataDir)
  } catch (error) {
    if (error instanceof Error && 'code' in error && error.code === 'ENOENT') entries = []
    else throw error
  }

  const initialized = entries.includes('config.json') && entries.includes('identity.json')
  if (!initialized) {
    if (entries.length !== 0) throw new Error('应用节点目录已有不完整数据，拒绝初始化或覆盖。')
    await run(program, { command: 'init', options: { data_dir: dataDir } })
  }

  const inspected = parseInspect(await run(program, { command: 'inspect', data_dir: dataDir }))
  if (!await sameDirectory(inspected.dataDir, dataDir) || inspected.status === 'unavailable') throw new Error('应用节点目录无法通过身份检查。')
  return inspected
}

/**
 * Coordinates current-user Node discovery and the client target independently from the
 * app-owned CoreSession. Candidate paths are observations only; every connection rechecks
 * the selected canonical path and Node identity through the fixed Core control channel.
 */
export class DesktopCoordinator {
  private preferences: ClientPreferences
  private discovery: NodeDiscovery = { kind: 'idle' }
  private backgroundNode: BackgroundNode = { kind: 'checking' }
  private initialized = false
  private tail: Promise<void> = Promise.resolve()
  private readonly platform: NodeJS.Platform
  private readonly discoverImpl: NonNullable<DesktopCoordinatorOptions['discover']>

  constructor(private readonly options: DesktopCoordinatorOptions) {
    this.preferences = { nodeMode: 'undecided', selectedNode: null, rememberedDataDirs: [] }
    this.platform = options.platform ?? process.platform
    this.discoverImpl = options.discover ?? discoverLocalNodes
  }

  async initialize(): Promise<void> {
    if (this.initialized) return
    this.preferences = await loadClientPreferences(this.options.profileDir)
    this.initialized = true
    await Promise.all([this.refreshBackground(), this.discover()])
  }

  status(): Pick<LauncherStatus, 'nodeMode' | 'selectedNode' | 'discovery' | 'backgroundNode'> {
    return {
      nodeMode: this.preferences.nodeMode,
      selectedNode: this.preferences.selectedNode,
      discovery: this.discovery,
      backgroundNode: this.backgroundNode,
    }
  }

  async discover(): Promise<void> {
    await this.serial(() => this.discoverInner())
  }

  private async discoverInner(): Promise<void> {
    this.discovery = { kind: 'scanning' }
    this.options.onChange?.()
    try {
      const remembered = [...new Set([...this.preferences.rememberedDataDirs,
        ...(this.preferences.selectedNode ? [this.preferences.selectedNode.dataDir] : [])])]
      const catalog = await this.discoverImpl(this.options.controlProgram, this.options.profileDir, remembered)
      this.discovery = { kind: 'ready', nodes: catalog.nodes, warnings: catalog.warnings }
      if (catalog.nodes.length && this.preferences.nodeMode === 'undecided') {
        this.preferences = { ...this.preferences, nodeMode: 'controller' }
      }
      const selected = await this.resolveCandidate(catalog.nodes, this.preferences.selectedNode)
      if (selected) {
        this.preferences = {
          ...this.preferences,
          nodeMode: this.preferences.nodeMode === 'undecided' ? 'controller' : this.preferences.nodeMode,
          selectedNode: { dataDir: selected.inspect.dataDir, deviceId: selected.inspect.deviceId },
        }
        await this.persist()
      } else {
        this.preferences = { ...this.preferences, selectedNode: null }
        const running = catalog.nodes.filter((node) => node.kind === 'running' && node.webEnabled)
        if (running.length === 1) {
          const candidate = running[0]
          if (candidate) {
            const resolved = await this.resolveCandidate(catalog.nodes, { dataDir: candidate.dataDir, deviceId: candidate.deviceId })
            if (resolved) {
              this.preferences = {
                ...this.preferences,
                nodeMode: this.preferences.nodeMode === 'undecided' ? 'controller' : this.preferences.nodeMode,
                selectedNode: { dataDir: resolved.inspect.dataDir, deviceId: resolved.inspect.deviceId },
              }
              await this.persist()
            }
          }
        }
        await this.persist()
      }
    } catch (error) {
      this.preferences = { ...this.preferences, selectedNode: null }
      this.discovery = { kind: 'failed', message: message(error) }
    }
    this.options.onChange?.()
  }

  async chooseMode(mode: 'controller' | 'local'): Promise<void> {
    await this.serial(async () => {
      this.preferences = { ...this.preferences, nodeMode: mode }
      await this.persist()
      this.changed()
    })
  }

  async startOwnedAppNode(): Promise<void> {
    await this.serial(async () => {
      await this.options.startOwnedAppNode()
      await this.discoverInner()
    })
  }

  async rememberDirectory(dataDir: string): Promise<void> {
    if (!isAbsolute(dataDir)) throw new Error('请选择已有节点的绝对路径。')
    await this.serial(async () => {
      const canonical = await realpath(dataDir)
      const inspected = await this.options.inspect(canonical)
      if (!await sameDirectory(inspected.dataDir, canonical) || !/^[a-f0-9]{64}$/.test(inspected.deviceId)) throw new Error('目录未能通过节点身份检查。')
      this.preferences = { ...this.preferences,
        nodeMode: this.preferences.nodeMode === 'undecided' ? 'controller' : this.preferences.nodeMode,
        rememberedDataDirs: [...new Set([...this.preferences.rememberedDataDirs, inspected.dataDir])].slice(-16),
      }
      await this.persist()
      await this.discoverInner()
    })
  }

  async clearSelection(): Promise<void> {
    await this.serial(async () => {
      this.preferences = { ...this.preferences, selectedNode: null, rememberedDataDirs: [] }
      await this.persist()
      this.changed()
    })
  }

  async connect(candidateId: string): Promise<void> {
    await this.serial(async () => {
      const node = this.candidate(candidateId)
      if (node.kind !== 'running') throw new Error('这个节点没有运行，请先明确选择启动。')
      const selected = await this.resolveCandidate([node], { dataDir: node.dataDir, deviceId: node.deviceId })
      if (!selected) throw new Error('节点已停止或身份发生变化，请重新扫描。')
      this.preferences = {
        ...this.preferences,
        selectedNode: { dataDir: selected.inspect.dataDir, deviceId: selected.inspect.deviceId },
        rememberedDataDirs: [...new Set([...this.preferences.rememberedDataDirs, selected.inspect.dataDir])].slice(-16),
      }
      await this.persist()
      this.changed()
    })
  }

  async start(candidateId: string): Promise<void> {
    await this.serial(async () => {
      const node = this.candidate(candidateId)
      if (node.kind !== 'stopped') throw new Error('这个节点不处于可启动的停止状态，请重新扫描。')
      if (node.sources.includes('user_service')) {
        const status = await this.options.managedNode.status()
        if (status.state !== 'installed_stopped' || status.device_id !== node.deviceId || status.data_dir !== node.dataDir) {
          throw new Error('这个用户服务不是 Vibeface 可验证和管理的服务，拒绝直接启动。')
        }
        let started: ManagedNodeStatus
        try { started = await this.options.managedNode.start(node.deviceId) }
        catch (error) { await this.refreshBackground(); throw error }
        const mapped = backgroundNodeFromStatus(started, this.platform)
        if (mapped.kind !== 'installed' || mapped.health !== 'running' || mapped.nodeId !== node.deviceId || mapped.dataDir !== node.dataDir) {
          throw new Error('用户服务启动后未能确认相同的节点身份。')
        }
        this.backgroundNode = mapped
      } else {
        await this.options.startDiscovered(node)
      }
      const verified = await this.resolveCandidate([], { dataDir: node.dataDir, deviceId: node.deviceId })
      if (!verified) throw new Error('节点启动后身份或健康状态校验失败。')
      this.preferences = { ...this.preferences, selectedNode: { dataDir: verified.inspect.dataDir, deviceId: verified.inspect.deviceId } }
      await this.persist()
      await this.discoverInner()
    })
  }

  async resolveSelectedNode(): Promise<SelectedLocalNode | null> {
    const selected = this.preferences.selectedNode
    if (!selected || !isAbsolute(selected.dataDir)) return null
    const resolved = await this.resolveCandidate([], selected)
    return resolved
  }

  async refreshBackground(): Promise<void> {
    this.backgroundNode = { kind: 'checking' }
    this.options.onChange?.()
    try {
      const status = await this.options.managedNode.status()
      this.backgroundNode = backgroundNodeFromStatus(status, this.platform)
    } catch (error) {
      this.backgroundNode = { kind: 'failure', message: message(error) }
    }
    this.options.onChange?.()
  }

  async installBackground(): Promise<void> {
    await this.serial(async () => {
      const existing = await this.options.managedNode.status()
      if (existing.state !== 'not_installed') throw new Error('后台节点服务已经存在或状态不明确，请先刷新状态。')
      const node = await this.options.prepareBackgroundNode()
      await this.options.beforeInstallBackground?.(node)
      const verified = await this.options.inspect(node.dataDir)
      if (verified.dataDir !== node.dataDir || verified.deviceId !== node.deviceId || verified.status !== 'stopped') {
        throw new Error('应用节点未能确认处于相同身份的停止状态，拒绝安装后台服务。')
      }
      let result: ManagedNodeStatus
      try { result = await this.options.managedNode.installBackground(node.deviceId) }
      catch (error) { await this.refreshBackground(); throw error }
      this.backgroundNode = backgroundNodeFromStatus(result, this.platform)
      if (this.backgroundNode.kind !== 'installed' || this.backgroundNode.nodeId !== node.deviceId || this.backgroundNode.dataDir !== node.dataDir) {
        throw new Error('后台节点安装后未能确认相同的节点身份。')
      }
      this.changed()
    })
  }

  async startBackground(): Promise<void> { await this.backgroundAction('start') }
  async stopBackground(): Promise<void> { await this.backgroundAction('stop') }
  async removeBackground(): Promise<void> { await this.backgroundAction('uninstall') }

  private async backgroundAction(action: 'start' | 'stop' | 'uninstall'): Promise<void> {
    await this.serial(async () => {
      const current = this.backgroundNode
      if (current.kind !== 'installed') throw new Error('没有可管理的已验证后台节点。')
      let result: ManagedNodeStatus
      try {
        result = action === 'start' ? await this.options.managedNode.start(current.nodeId)
          : action === 'stop' ? await this.options.managedNode.stop(current.nodeId)
            : await this.options.managedNode.uninstall(current.nodeId)
      } catch (error) {
        await this.refreshBackground()
        throw error
      }
      this.backgroundNode = backgroundNodeFromStatus(result, this.platform)
      await this.refreshBackground()
      await this.discoverInner()
      this.changed()
    })
  }

  private async resolveCandidate(nodes: readonly LocalNode[], expected: { dataDir: string; deviceId: string } | null): Promise<SelectedLocalNode | null> {
    if (!expected) return null
    if (nodes.length && !nodes.some((node) => node.dataDir === expected.dataDir && node.deviceId === expected.deviceId && node.kind === 'running')) return null
    try {
      const inspected = await this.options.inspect(expected.dataDir)
      if (!await sameDirectory(inspected.dataDir, expected.dataDir) || inspected.deviceId !== expected.deviceId || inspected.status !== 'running' || !inspected.webEnabled || inspected.publicEntry) return null
      const access = await this.options.access(inspected.dataDir)
      if (access.deviceId !== inspected.deviceId || access.kind !== 'local') return null
      return { inspect: inspected, access }
    } catch { return null }
  }

  private candidate(id: string): LocalNode {
    if (this.discovery.kind !== 'ready') throw new Error('请先完成节点扫描。')
    const candidate = this.discovery.nodes.find((node) => node.id === id)
    if (!candidate) throw new Error('节点候选已过期，请重新扫描。')
    return candidate
  }

  private async persist(): Promise<void> { await saveClientPreferences(this.options.profileDir, this.preferences) }
  private changed(): void { this.options.onChange?.() }

  private async serial(work: () => Promise<void>): Promise<void> {
    let release!: () => void
    const previous = this.tail
    this.tail = new Promise<void>((resolve) => { release = resolve })
    await previous
    try { await work() } finally { release() }
  }
}
