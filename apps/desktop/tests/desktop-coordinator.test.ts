import { mkdtemp, mkdir, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { DesktopCoordinator, backgroundNodeFromStatus } from '../src/main/desktop-coordinator.js'
import { parseLocalNodes, type LocalNodes } from '../src/main/local-nodes.js'
import type { ManagedNodeController, ManagedNodeStatus } from '../src/main/managed-node.js'
import type { Inspect } from '../src/main/control-types.js'

const deviceId = 'a'.repeat(64)
const inspect = (dataDir: string, id = deviceId, status: Inspect['status'] = 'running'): Inspect => ({
  status, deviceId: id, dataDir, localListen: '127.0.0.1:4791', listen: '0.0.0.0:4790',
  webEnabled: true, publicEntry: false, coreVersion: '0.0.1-nightly.20261107.1', error: null,
})
const local = (dataDir: string, id = deviceId, status = 'running') => ({ status, data_dir: dataDir, device_id: id,
  local_listen: '127.0.0.1:4791', listen: '0.0.0.0:4790', web_enabled: true,
  core_version: '0.0.1-nightly.20261107.1', sources: ['remembered'] })
const noNode: ManagedNodeStatus = { ok: true, state: 'not_installed', device_id: null, data_dir: null,
  service_id: null, managed_root: null, service_running: null, core_status: null }

let roots: string[] = []
afterEach(async () => { await Promise.all(roots.map(path => rm(path, { recursive: true, force: true }))); roots = [] })
async function temp(): Promise<string> { const path = await mkdtemp(join(tmpdir(), 'desktop-coordinator-')); roots.push(path); return path }

function coordinator(profileDir: string, rows: unknown[], identities: Map<string, Inspect>, startDiscovered = vi.fn()) {
  const catalog: LocalNodes = parseLocalNodes({ nodes: rows, warnings: [] })
  const managed = { status: vi.fn().mockResolvedValue(noNode), installBackground: vi.fn(), start: vi.fn(), stop: vi.fn(), uninstall: vi.fn() } as unknown as ManagedNodeController
  const value = new DesktopCoordinator({
    profileDir, controlProgram: { command: '/fixed/core', args: ['--control'] },
    discover: async () => catalog,
    inspect: async (path) => { const result = identities.get(path); if (!result) throw new Error('missing'); return result },
    access: async (path) => ({ kind: 'local', url: 'http://127.0.0.1:4791/#local=secret', deviceId: identities.get(path)?.deviceId ?? '' }),
    managedNode: managed, startDiscovered,
  })
  return { value, managed }
}

describe('Desktop coordinator', () => {
  it('auto-selects a single verified running node and keeps controller choice independent', async () => {
    const profile = await temp(), dataDir = await temp()
    const instance = coordinator(profile, [local(dataDir)], new Map([[dataDir, inspect(dataDir)]]))
    await instance.value.initialize()
    expect(instance.value.status().nodeMode).toBe('controller')
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
      service_id: 'desktop-profile', managed_root: '/managed', service_running: true, core_status: 'stopped' }
    expect(backgroundNodeFromStatus(installed, 'darwin')).toMatchObject({ kind: 'installed', health: 'unhealthy', manager: 'launchd' })
  })
})
