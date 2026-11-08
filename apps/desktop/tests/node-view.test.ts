import { describe, expect, it } from 'vitest'
import { describeNodeSettings as toView } from '../src/shared/node-view.js'
import type { BackgroundNode, CoreState, DesktopStatus, LocalNode } from '../src/shared/contract.js'

const status = (overrides: Partial<DesktopStatus> = {}): DesktopStatus => ({
  runtime: { kind: 'ready', corePath: '/app/resources/runtime/vibeface-core' },
  core: { kind: 'none' },
  hostPhase: 'ready',
  clientVisible: true,
  preferences: { openBusinessOnConnect: true },
  profileDir: '/tmp/profile',
  version: '0.0.1-nightly.20261107.1',
  attachOnly: false,
  clientAvailable: true,
  nodeMode: 'undecided',
  selectedNode: null,
  discovery: { kind: 'idle' },
  backgroundNode: { kind: 'absent' },
  ...overrides,
})

const node = (kind: LocalNode['kind'], id = 'candidate-1'): LocalNode => {
  const identity = { id, deviceId: 'a'.repeat(64), dataDir: '/Users/example/.vibeface', sources: ['default_home'] as const }
  if (kind === 'unavailable') return { ...identity, sources: [...identity.sources], kind, reason: 'inspection_unavailable' }
  return {
    ...identity, sources: [...identity.sources], kind,
    localListen: '127.0.0.1:4790', listen: '127.0.0.1:4790', webEnabled: true, coreVersion: '0.0.1',
  }
}

describe('desktop node settings view', () => {
  it('requires a stop before continuing recovery and never offers a pending start or removal', () => {
    const stopped = node('stopped')
    const runtimeUpdate = { bundledVersion: '0.0.1', installedVersion: '0.0.1', runningVersion: null,
      availability: 'current' as const, recoveryRequired: true }
    const backgroundNode: BackgroundNode = { kind: 'installed', nodeId: stopped.deviceId, dataDir: stopped.dataDir,
      health: 'stopped', manager: 'launchd', runtimeUpdate }
    const offline = toView(status({ nodeMode: 'local', backgroundNode, discovery: { kind: 'ready', nodes: [stopped], warnings: [] } }))
    expect(offline.backgroundActions.map(a => a.id)).toEqual(['update-background'])
    expect(offline.backgroundActions[0]).toMatchObject({ label: '继续更新', disabled: false })
    expect(offline.rows[0]?.actions).toEqual([])
    const running = toView(status({ nodeMode: 'local', backgroundNode: { ...backgroundNode, health: 'running' } }))
    expect(running.backgroundActions.map(a => a.id)).toEqual(['update-background', 'stop-background'])
    expect(running.backgroundActions[0]?.disabled).toBe(true)
    expect(running.backgroundMessage).toContain('先停止后台节点')
  })

  it('offers controller-only and local-node choices on first open', () => {
    expect(toView(status()).actions.map((item) => item.id)).toEqual(['controller', 'local'])
  })

  it('still describes undecided mode while the desktop host is starting', () => {
    expect(toView(status({ hostPhase: 'starting' })).headline).toBe('Vibeface')
  })

  it('keeps the controller available without a Core runtime and discovers healthy choices', () => {
    const view = toView(status({
      nodeMode: 'controller',
      runtime: { kind: 'missing', corePath: '/missing', detail: 'Core unavailable' },
      discovery: { kind: 'ready', nodes: [node('running'), node('running', 'candidate-2')], warnings: [] },
    }))
    expect(view.actions.map((item) => item.id)).toContain('open')
    expect(view.rows.map((row) => row.actions[0]?.id)).toEqual(['connect', 'connect'])
    expect(view.rows[0]?.actions[0]?.nodeId).toBe('candidate-1')
  })

  it('requires an explicit action to start a stopped discovered node', () => {
    const view = toView(status({ nodeMode: 'controller', discovery: { kind: 'ready', nodes: [node('stopped')], warnings: [] } }))
    expect(view.rows[0]?.actions.map((item) => item.id)).toEqual(['start-discovered'])
  })

  it('shows bounded unavailable observations without offering ownership actions', () => {
    const view = toView(status({ nodeMode: 'controller', discovery: { kind: 'ready', nodes: [node('unavailable')], warnings: [] } }))
    expect(view.rows[0]?.detail).toContain('重新检查')
    expect(view.rows[0]?.actions.map((item) => item.id)).toEqual(['discover'])
    expect(view.rows[0]?.actions.some((item) => /stop|remove/.test(item.id))).toBe(false)
  })

  it('separates App child stop controls from connected Nodes', () => {
    const core: CoreState = { kind: 'owned', dataDir: '/tmp/node', deviceId: 'a', origin: 'http://127.0.0.1:4791' }
    expect(toView(status({ nodeMode: 'local', core })).actions.map((item) => item.id)).toContain('stop-owned')
    const attached: CoreState = { ...core, kind: 'attached' }
    expect(toView(status({ nodeMode: 'local', core: attached })).actions.map((item) => item.id)).not.toContain('stop-owned')
  })

  it('shows the selected client target while preserving a different App-owned Node', () => {
    const core: CoreState = { kind: 'owned', dataDir: '/app/node', deviceId: 'app-node', origin: 'http://127.0.0.1:4791' }
    const selectedNode = { dataDir: '/existing/node', deviceId: 'existing-node' }
    const view = toView(status({ nodeMode: 'controller', core, selectedNode }))
    expect(view.headline).toBe('已选择本机节点')
    expect(view.facts).toContainEqual(['访问节点目录', '/existing/node'])
    expect(view.facts).toContainEqual(['运行目录', '/app/node'])
    expect(view.actions.map(item => item.id)).toContain('stop-owned')
    expect(view.actions.map(item => item.id)).toContain('local')
  })

  it('shows installed background ownership even in controller mode and never offers its removal there', () => {
    const backgroundNode: BackgroundNode = { kind: 'installed', nodeId: 'node-1', dataDir: '/var/data/node', health: 'running', manager: 'launchd',
      runtimeUpdate: { bundledVersion: '0.0.1', installedVersion: '0.0.1', runningVersion: '0.0.1', recoveryRequired: false, availability: 'current' } }
    const view = toView(status({ nodeMode: 'controller', backgroundNode }))
    expect(view.backgroundMessage).toContain('正在运行')
    expect(view.backgroundActions).toEqual([])
  })

  it('offers truthful background service controls only when local hosting is selected', () => {
    const runtime = { bundledVersion: '0.0.1-nightly.20261108.1', installedVersion: '0.0.1-nightly.20261107.1', runningVersion: null, recoveryRequired: false, availability: 'current' as const }
    const backgroundNode: BackgroundNode = { kind: 'installed', nodeId: 'node-1', dataDir: '/var/data/node', health: 'stopped', manager: 'launchd', runtimeUpdate: runtime }
    expect(toView(status({ nodeMode: 'local', backgroundNode })).backgroundActions.map((item) => item.id)).toEqual(['start-background', 'remove-background'])
    expect(toView(status({ nodeMode: 'local', backgroundNode: { kind: 'conflict', message: '服务注册已更改' } })).backgroundActions).toEqual([])
    const partial: BackgroundNode = { kind: 'partial', nodeId: 'node-1', dataDir: '/var/data/node', message: '后台服务注册不完整',
      runtimeUpdate: { bundledVersion: '0.0.1', installedVersion: '0.0.1', runningVersion: null, recoveryRequired: false, availability: 'current' } }
    expect(toView(status({ nodeMode: 'local', backgroundNode: partial })).backgroundActions.map(item => item.id)).toEqual(['discover', 'remove-background'])
  })

  it('offers a primary update action only when the bundled runtime is newer', () => {
    const runtimeUpdate = { bundledVersion: '0.0.1-nightly.20261108.1', installedVersion: '0.0.1-nightly.20261107.1', runningVersion: '0.0.1-nightly.20261107.1', recoveryRequired: false, availability: 'newer_available' as const }
    const backgroundNode: BackgroundNode = { kind: 'installed', nodeId: 'node-1', dataDir: '/var/data/node', health: 'running', manager: 'launchd', runtimeUpdate }
    const view = toView(status({ nodeMode: 'local', backgroundNode }))
    expect(view.backgroundActions.map((item) => item.id)).toEqual(['update-background', 'stop-background', 'remove-background'])
    expect(view.backgroundActions[0]?.primary).toBe(true)
    expect(view.backgroundMessage).toContain('0.0.1-nightly.20261108.1')
  })

  it('keeps client action present while local Core is missing and reports discovery failure separately', () => {
    const view = toView(status({
      nodeMode: 'controller', clientAvailable: true,
      runtime: { kind: 'missing', corePath: '/missing', detail: 'Bundled Core missing' },
      discovery: { kind: 'failed', message: '本机节点检查失败，可重新检查。' },
    }))
    expect(view.actions.map((item) => item.id)).toContain('open')
    expect(view.discoveryMessage).toContain('失败')
  })
})
