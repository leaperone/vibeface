import { mkdtemp, mkdir, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve, toNamespacedPath } from 'node:path'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { DesktopCoordinator, backgroundNodeFromStatus, prepareAppProfileNode } from '../src/main/desktop-coordinator.js'
import { parseLocalNodes, type LocalNodes } from '../src/main/local-nodes.js'
import type { ManagedNodeController, ManagedNodeRuntimeUpdate, ManagedNodeStatus } from '../src/main/managed-node.js'
import type { ControlRequest, Inspect } from '../src/main/control-types.js'

const deviceId = 'a'.repeat(64)
const inspect = (dataDir: string, id = deviceId, status: Inspect['status'] = 'running'): Inspect => ({
  status, deviceId: id, dataDir, localListen: '127.0.0.1:4791', listen: '0.0.0.0:4790',
  webEnabled: true, publicEntry: false, coreVersion: '0.0.1-nightly.20261107.1', error: null,
})
const local = (dataDir: string, id = deviceId, status = 'running') => ({ status, data_dir: dataDir, device_id: id,
  local_listen: '127.0.0.1:4791', listen: '0.0.0.0:4790', web_enabled: true,
  core_version: '0.0.1-nightly.20261107.1', sources: ['remembered'] })
const runtime = (availability: ManagedNodeRuntimeUpdate['availability'], overrides: Partial<ManagedNodeRuntimeUpdate> = {}): ManagedNodeRuntimeUpdate => ({
  bundledVersion: '0.0.1-nightly.20261108.1',
  installedVersion: availability === 'not_installed' ? null : '0.0.1-nightly.20261107.1',
  runningVersion: availability === 'not_installed' ? null : '0.0.1-nightly.20261107.1',
  availability,
  recoveryRequired: false,
  ...overrides,
})
const noNode: ManagedNodeStatus = { ok: true, state: 'not_installed', device_id: null, data_dir: null,
  service_id: null, managed_root: null, service_running: null, core_status: null, runtime_update: runtime('not_installed') }

let roots: string[] = []
afterEach(async () => { await Promise.all(roots.map(path => rm(path, { recursive: true, force: true }))); roots = [] })
async function temp(): Promise<string> { const path = await mkdtemp(join(tmpdir(), 'desktop-coordinator-')); roots.push(path); return path }

function coordinator(profileDir: string, rows: unknown[], identities: Map<string, Inspect>, startDiscovered = vi.fn()) {
  const catalog: LocalNodes = parseLocalNodes({ nodes: rows, warnings: [] })
  const managed = { status: vi.fn().mockResolvedValue(noNode), installBackground: vi.fn(), updateBackground: vi.fn(), start: vi.fn(), stop: vi.fn(), uninstall: vi.fn() } as unknown as ManagedNodeController
  const value = new DesktopCoordinator({
    profileDir, controlProgram: { command: '/fixed/core', args: ['--control'] },
    discover: async () => catalog,
    inspect: async (path) => { const result = identities.get(path); if (!result) throw new Error('missing'); return result },
    access: async (path) => ({ kind: 'local', url: 'http://127.0.0.1:4791/#local=secret', deviceId: identities.get(path)?.deviceId ?? '' }),
    prepareBackgroundNode: async () => { throw new Error('unused in this test') },
    managedNode: managed, startDiscovered, startOwnedAppNode: async () => undefined,
  })
  return { value, managed }
}

describe('Desktop coordinator', () => {
  it('auto-selects a single verified running node and keeps controller choice independent', async () => {
    const profile = await temp(), dataDir = await temp()
    const instance = coordinator(profile, [local(dataDir)], new Map([[dataDir, inspect(dataDir)]]))
    await instance.value.initialize()
    expect(instance.value.status().nodeMode).toBe('controller')
    expect(instance.value.status().selectedNode).toEqual({ dataDir, deviceId })
    expect(await instance.value.resolveSelectedNode()).toMatchObject({ inspect: { dataDir, deviceId } })
    await instance.value.chooseMode('controller')
    expect(await instance.value.resolveSelectedNode()).toMatchObject({ inspect: { deviceId } })
    expect(instance.value.status().discovery.kind).toBe('ready')
  })

  it('shows multiple running nodes without choosing one and requires explicit start for stopped nodes', async () => {
    const profile = await temp(), first = await temp(), second = await temp()
    const start = vi.fn()
    const instance = coordinator(profile, [local(first), local(second, 'b'.repeat(64)), local(resolve(profile, 'stopped'), 'c'.repeat(64), 'stopped')],
      new Map([[first, inspect(first)], [second, inspect(second, 'b'.repeat(64))]]), start)
    await instance.value.initialize()
    expect(await instance.value.resolveSelectedNode()).toBeNull()
    expect(instance.value.status().nodeMode).toBe('controller')
    const stopped = instance.value.status().discovery
    expect(stopped.kind).toBe('ready')
    if (stopped.kind !== 'ready') return
    const candidate = stopped.nodes.find(node => node.kind === 'stopped')
    expect(candidate).toBeDefined()
    await expect(instance.value.connect(candidate!.id)).rejects.toThrow(/明确选择启动/)
    expect(start).not.toHaveBeenCalled()
  })

  it('rejects stale identity and maps only matching service and Core health as healthy', async () => {
    const profile = await temp(), dataDir = await temp()
    const instance = coordinator(profile, [local(dataDir)], new Map([[dataDir, inspect(dataDir, 'b'.repeat(64))]]))
    await instance.value.initialize()
    expect(await instance.value.resolveSelectedNode()).toBeNull()
    const installed: ManagedNodeStatus = { ok: true, state: 'installed', device_id: deviceId, data_dir: dataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: true, core_status: 'stopped',
      runtime_update: runtime('current') }
    expect(backgroundNodeFromStatus(installed, 'darwin')).toMatchObject({ kind: 'installed', health: 'unhealthy', manager: 'launchd' })
  })

  it('clears a previously selected Node when its on-disk identity changes', async () => {
    const profile = await temp(), dataDir = await temp()
    const identities = new Map([[dataDir, inspect(dataDir)]])
    const instance = coordinator(profile, [local(dataDir)], identities)
    await instance.value.initialize()
    expect(instance.value.status().selectedNode?.deviceId).toBe(deviceId)
    identities.set(dataDir, inspect(dataDir, 'b'.repeat(64)))
    await instance.value.discover()
    expect(instance.value.status().selectedNode).toBeNull()
    expect(await instance.value.resolveSelectedNode()).toBeNull()
    expect(instance.managed.start).not.toHaveBeenCalled()
    expect(instance.managed.stop).not.toHaveBeenCalled()
  })

  it('recognizes one directory across the native inspector and JavaScript path spellings', async () => {
    const profile = await temp(), dataDir = await temp()
    const canonical = toNamespacedPath(await realpath(dataDir))
    const identities = new Map([[dataDir, inspect(canonical)], [canonical, inspect(canonical)]])
    const instance = coordinator(profile, [local(dataDir)], identities)
    await instance.value.initialize()
    expect(instance.value.status().selectedNode).toEqual({ dataDir: canonical, deviceId })
    expect(await instance.value.resolveSelectedNode()).toMatchObject({ inspect: { dataDir: canonical, deviceId } })
  })

  it('starts an installed App-profile Node through its service rather than taking child ownership', async () => {
    const profile = await temp(), dataDir = await realpath(await temp())
    let running = false
    const installed: ManagedNodeStatus = { ok: true, state: 'installed_stopped', device_id: deviceId, data_dir: dataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: false, core_status: 'stopped',
      runtime_update: runtime('current') }
    const managed = { status: vi.fn(async () => installed),
      start: vi.fn(async () => { running = true; return { ...installed, state: 'installed', service_running: true, core_status: 'running' } }),
      stop: vi.fn(), installBackground: vi.fn(), uninstall: vi.fn(), updateBackground: vi.fn() } as unknown as ManagedNodeController
    const startChild = vi.fn()
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] }, managedNode: managed,
      discover: async () => parseLocalNodes({ nodes: [{ ...local(dataDir, deviceId, running ? 'running' : 'stopped'), sources: ['app_profile'] }], warnings: [] }),
      inspect: async () => inspect(dataDir, deviceId, running ? 'running' : 'stopped'),
      access: async () => ({ kind: 'local', url: 'http://127.0.0.1:4791/#local=secret', deviceId }),
      prepareBackgroundNode: async () => { throw new Error('unused') },
      startDiscovered: startChild, startOwnedAppNode: startChild,
    })
    await subject.initialize()
    const catalog = subject.status().discovery
    expect(catalog.kind).toBe('ready')
    if (catalog.kind !== 'ready') throw new Error('expected stopped Node')
    const candidate = catalog.nodes[0]
    if (!candidate) throw new Error('expected stopped Node')
    await subject.start(candidate.id)
    expect(managed.start).toHaveBeenCalledWith(deviceId)
    expect(startChild).not.toHaveBeenCalled()
    expect(subject.status().selectedNode).toEqual({ dataDir, deviceId })
    await expect(subject.startOwnedAppNode()).rejects.toThrow(/后台服务管理/)
    expect(startChild).not.toHaveBeenCalled()
  })

  it('reconciles a partial removal and retries only the same verified Node', async () => {
    const profile = await temp(), dataDir = await temp()
    const partial: ManagedNodeStatus = { ok: true, state: 'partial', device_id: deviceId, data_dir: dataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: false, core_status: 'stopped',
      runtime_update: runtime('current') }
    const status = vi.fn().mockResolvedValueOnce(partial).mockResolvedValueOnce(partial).mockResolvedValue(noNode)
    const uninstall = vi.fn().mockResolvedValue(noNode)
    const managed = { status, uninstall } as unknown as ManagedNodeController
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] }, managedNode: managed,
      discover: async () => ({ nodes: [], warnings: [] }),
      inspect: async () => { throw new Error('unused') }, access: async () => { throw new Error('unused') },
      prepareBackgroundNode: async () => { throw new Error('unused') },
      startDiscovered: vi.fn(), startOwnedAppNode: vi.fn(),
    })
    await subject.initialize()
    expect(subject.status().backgroundNode).toMatchObject({ kind: 'partial', nodeId: deviceId, dataDir })
    await subject.removeBackground()
    expect(uninstall).toHaveBeenCalledTimes(1)
    expect(uninstall).toHaveBeenCalledWith(deviceId)
    expect(subject.status().backgroundNode.kind).toBe('absent')

    status.mockReset().mockResolvedValueOnce(partial).mockResolvedValue({ ...partial, device_id: 'b'.repeat(64) })
    uninstall.mockClear()
    const replaced = new DesktopCoordinator({
      profileDir: await temp(), controlProgram: { command: '/fixed/core', args: ['--control'] }, managedNode: managed,
      discover: async () => ({ nodes: [], warnings: [] }),
      inspect: async () => { throw new Error('unused') }, access: async () => { throw new Error('unused') },
      prepareBackgroundNode: async () => { throw new Error('unused') },
      startDiscovered: vi.fn(), startOwnedAppNode: vi.fn(),
    })
    await replaced.initialize()
    await expect(replaced.removeBackground()).rejects.toThrow(/身份或服务状态已变化/)
    expect(uninstall).not.toHaveBeenCalled()
  })

  it('remembers only an existing inspected directory and forgets the client target without stopping a Node', async () => {
    const profile = await temp(), dataDir = await realpath(await temp())
    const instance = coordinator(profile, [local(dataDir)], new Map([[dataDir, inspect(dataDir)]]))
    await instance.value.initialize()
    await instance.value.rememberDirectory(dataDir)
    const stored = JSON.parse(await readFile(join(profile, 'client-preferences.json'), 'utf8'))
    expect(stored.rememberedDataDirs).toEqual([await realpath(dataDir)])
    await instance.value.clearSelection()
    expect(instance.value.status().selectedNode).toBeNull()
    expect(instance.managed.stop).not.toHaveBeenCalled()
    expect(instance.managed.uninstall).not.toHaveBeenCalled()
    await expect(instance.value.rememberDirectory(join(dataDir, 'missing'))).rejects.toThrow()
  })

  it('initializes a fresh App node, inspects it stopped, then installs the same identity without starting a child', async () => {
    const profile = await temp(), dataDir = join(profile, 'node'), selectedDir = await temp()
    const events: string[] = []
    const control = vi.fn(async (_program: { command: string; args: readonly string[] }, request: ControlRequest): Promise<unknown> => {
      if (request.command === 'init') {
        events.push('init')
        await mkdir(dataDir, { recursive: true })
        await writeFile(join(dataDir, 'config.json'), '{}')
        await writeFile(join(dataDir, 'identity.json'), '{}')
        return null
      }
      if (request.command !== 'inspect') throw new Error(`unexpected ${request.command}`)
      events.push('inspect')
      const value = inspect(dataDir, deviceId, 'stopped')
      return { status: value.status, device_id: value.deviceId, data_dir: await realpath(value.dataDir),
        local_listen: value.localListen, listen: value.listen, web_enabled: value.webEnabled,
        public_entry: value.publicEntry, core_version: value.coreVersion, error: null }
    })
    const prepared = await prepareAppProfileNode(profile, { command: '/fixed/core', args: ['--control'] }, control as never)
    expect(prepared).toMatchObject({ dataDir: await realpath(dataDir), deviceId, status: 'stopped' })

    const canonicalDataDir = await realpath(dataDir)
    const selectedId = 'b'.repeat(64)
    const installed: ManagedNodeStatus = { ok: true, state: 'installed', device_id: deviceId, data_dir: canonicalDataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: true, core_status: 'running',
      runtime_update: runtime('current') }
    const managed = {
      status: vi.fn().mockResolvedValue(noNode),
      installBackground: vi.fn(async () => { events.push('install'); return installed }),
      start: vi.fn(), stop: vi.fn(), uninstall: vi.fn(), updateBackground: vi.fn(),
    } as unknown as ManagedNodeController
    const runChild = vi.fn()
    const access = vi.fn(async (path: string) => ({ kind: 'local' as const, url: 'http://127.0.0.1:4791/#local=secret', deviceId: path === selectedDir ? selectedId : deviceId }))
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] },
      discover: async () => parseLocalNodes({ nodes: [local(selectedDir, selectedId)], warnings: [] }),
      inspect: async (path) => {
        if (path === selectedDir) return inspect(selectedDir, selectedId)
        events.push('verify-stopped')
        return inspect(canonicalDataDir, deviceId, 'stopped')
      },
      access,
      prepareBackgroundNode: async () => prepared,
      beforeInstallBackground: async () => { events.push('handoff-check') },
      managedNode: managed, startDiscovered: runChild, startOwnedAppNode: async () => undefined,
    })
    await subject.initialize()
    expect(await subject.resolveSelectedNode()).toMatchObject({ inspect: { dataDir: selectedDir, deviceId: selectedId } })
    access.mockClear()
    await subject.installBackground()
    expect(events).toEqual(['init', 'inspect', 'handoff-check', 'verify-stopped', 'install'])
    expect(managed.installBackground).toHaveBeenCalledWith(deviceId)
    expect(runChild).not.toHaveBeenCalled()
    expect((managed.start as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled()
    expect(access).not.toHaveBeenCalled()
    expect(await subject.resolveSelectedNode()).toMatchObject({ inspect: { dataDir: selectedDir, deviceId: selectedId } })
    expect(subject.status().backgroundNode).toMatchObject({ kind: 'installed', nodeId: deviceId, dataDir: canonicalDataDir, health: 'running' })
  })

  it('waits for its owned App child to stop before installing the same Node as a service', async () => {
    const profile = await temp(), dataDir = await temp()
    const events: string[] = []
    let stopped = false
    const canonicalDataDir = await realpath(dataDir)
    const running = inspect(canonicalDataDir, deviceId, 'running')
    const installed: ManagedNodeStatus = { ok: true, state: 'installed', device_id: deviceId, data_dir: canonicalDataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: true, core_status: 'running',
      runtime_update: runtime('current') }
    const managed = {
      status: vi.fn().mockResolvedValue(noNode),
      installBackground: vi.fn(async () => { events.push('install'); return installed }),
      start: vi.fn(), stop: vi.fn(), uninstall: vi.fn(), updateBackground: vi.fn(),
    } as unknown as ManagedNodeController
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] },
      discover: async () => ({ nodes: [], warnings: [] }),
      prepareBackgroundNode: async () => running,
      beforeInstallBackground: async (node) => {
        expect(node).toMatchObject({ dataDir: canonicalDataDir, deviceId, status: 'running' })
        await new Promise(resolve => setTimeout(resolve, 5))
        stopped = true
        events.push('owned-child-exit')
      },
      inspect: async () => {
        expect(stopped).toBe(true)
        events.push('verify-stopped')
        return inspect(canonicalDataDir, deviceId, 'stopped')
      },
      access: async () => { throw new Error('installation must not need client access') },
      managedNode: managed, startDiscovered: vi.fn(), startOwnedAppNode: async () => undefined,
    })
    await subject.initialize()
    await subject.installBackground()
    expect(events).toEqual(['owned-child-exit', 'verify-stopped', 'install'])
    expect(managed.installBackground).toHaveBeenCalledWith(deviceId)
    expect(subject.status().backgroundNode).toMatchObject({ kind: 'installed', nodeId: deviceId, dataDir: canonicalDataDir })
  })

  it('updates a stopped managed Node without starting it, only when bundled runtime is newer', async () => {
    const profile = await temp(), dataDir = await temp()
    const stale: ManagedNodeStatus = { ok: true, state: 'installed_stopped', device_id: deviceId, data_dir: dataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: false, core_status: 'stopped',
      runtime_update: runtime('newer_available') }
    const updated: ManagedNodeStatus = { ...stale,
      runtime_update: runtime('current', { installedVersion: '0.0.1-nightly.20261108.1', runningVersion: null }) }
    const managed = {
      status: vi.fn().mockResolvedValueOnce(stale).mockResolvedValueOnce(stale).mockResolvedValue(updated),
      updateBackground: vi.fn(async () => updated),
      installBackground: vi.fn(), start: vi.fn(), stop: vi.fn(), uninstall: vi.fn(),
    } as unknown as ManagedNodeController
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] }, managedNode: managed,
      discover: async () => ({ nodes: [], warnings: [] }),
      inspect: async () => { throw new Error('unused') }, access: async () => { throw new Error('unused') },
      prepareBackgroundNode: async () => { throw new Error('unused') },
      startDiscovered: vi.fn(), startOwnedAppNode: vi.fn(),
    })
    await subject.initialize()
    await subject.updateBackground()
    expect(managed.updateBackground).toHaveBeenCalledWith(deviceId)
    expect(subject.status().backgroundNode).toMatchObject({
      kind: 'installed', health: 'stopped', runtimeUpdate: { availability: 'current', installedVersion: '0.0.1-nightly.20261108.1', runningVersion: null },
    })
    await expect(subject.updateBackground()).rejects.toThrow(/没有可更新的版本/)
  })
  it.each(['identity', 'running-state', 'version'] as const)('refuses an update acknowledgement with an unexpected %s', async (failure) => {
    const profile = await temp(), dataDir = await temp()
    const stale: ManagedNodeStatus = { ok: true, state: 'installed_stopped', device_id: deviceId, data_dir: dataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: false, core_status: 'stopped',
      runtime_update: runtime('newer_available') }
    const result: ManagedNodeStatus = { ...stale,
      runtime_update: runtime('current', { installedVersion: '0.0.1-nightly.20261108.1', runningVersion: null }) }
    if (failure === 'identity') result.device_id = 'b'.repeat(64)
    if (failure === 'version') result.runtime_update = runtime('newer_available')
    if (failure === 'running-state') {
      result.state = 'installed'
      result.service_running = true
      result.core_status = 'running'
      result.runtime_update.runningVersion = result.runtime_update.bundledVersion
    }
    const managed = {
      status: vi.fn().mockResolvedValue(stale), updateBackground: vi.fn().mockResolvedValue(result),
      installBackground: vi.fn(), start: vi.fn(), stop: vi.fn(), uninstall: vi.fn(),
    } as unknown as ManagedNodeController
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] }, managedNode: managed,
      discover: async () => ({ nodes: [], warnings: [] }),
      inspect: async () => { throw new Error('unused') }, access: async () => { throw new Error('unused') },
      prepareBackgroundNode: async () => { throw new Error('unused') },
      startDiscovered: vi.fn(), startOwnedAppNode: vi.fn(),
    })
    await subject.initialize()
    await expect(subject.updateBackground()).rejects.toThrow(failure === 'identity' ? /节点身份/ : /运行状态或版本/)
    expect(managed.installBackground).not.toHaveBeenCalled()
    expect(managed.start).not.toHaveBeenCalled()
    expect(managed.stop).not.toHaveBeenCalled()
  })

  it('requires an explicit stop before recovering the same bundled version and keeps it stopped', async () => {
    const profile = await temp(), dataDir = await temp()
    let observed: ManagedNodeStatus = { ok: true, state: 'installed', device_id: deviceId, data_dir: dataDir,
      service_id: 'desktop-profile', managed_root: '/managed', service_running: true, core_status: 'running',
      runtime_update: runtime('current', { recoveryRequired: true, installedVersion: '0.0.1-nightly.20261108.1', runningVersion: '0.0.1-nightly.20261108.1' }) }
    const managed = {
      status: vi.fn(async () => observed),
      stop: vi.fn(async () => { observed = { ...observed, state: 'installed_stopped', service_running: false, core_status: 'stopped',
        runtime_update: { ...observed.runtime_update, runningVersion: null } }; return observed }),
      updateBackground: vi.fn(async () => { observed = { ...observed, runtime_update: { ...observed.runtime_update, recoveryRequired: false } }; return observed }),
      start: vi.fn(), installBackground: vi.fn(), uninstall: vi.fn(),
    }
    const subject = new DesktopCoordinator({
      profileDir: profile, controlProgram: { command: '/fixed/core', args: ['--control'] },
      managedNode: managed as unknown as ManagedNodeController,
      discover: async () => ({ nodes: [], warnings: [] }),
      inspect: async () => { throw new Error('unused') }, access: async () => { throw new Error('unused') },
      prepareBackgroundNode: async () => { throw new Error('unused') }, startDiscovered: vi.fn(), startOwnedAppNode: vi.fn(),
    })
    await subject.initialize()
    await expect(subject.updateBackground()).rejects.toThrow(/先停止/)
    await expect(subject.startBackground()).rejects.toThrow(/继续更新/)
    await expect(subject.removeBackground()).rejects.toThrow(/继续更新/)
    expect(managed.updateBackground).not.toHaveBeenCalled()
    await subject.stopBackground()
    expect(subject.status().backgroundNode).toMatchObject({ health: 'stopped', runtimeUpdate: { recoveryRequired: true } })
    await subject.updateBackground()
    expect(subject.status().backgroundNode).toMatchObject({ health: 'stopped', nodeId: deviceId, runtimeUpdate: { availability: 'current', recoveryRequired: false } })
    expect(managed.stop).toHaveBeenCalledTimes(1)
    expect(managed.updateBackground).toHaveBeenCalledTimes(1)
    expect(managed.start).not.toHaveBeenCalled()
    expect(managed.installBackground).not.toHaveBeenCalled()
    expect(managed.uninstall).not.toHaveBeenCalled()
  })

})
