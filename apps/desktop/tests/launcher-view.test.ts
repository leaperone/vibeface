import { describe, expect, it } from 'vitest'
import { describe as toView } from '../src/launcher/view.js'
import type { CoreState, LauncherStatus } from '../src/shared/contract.js'

const status = (core: CoreState, overrides: Partial<LauncherStatus> = {}): LauncherStatus => ({
  runtime: { kind: 'ready', corePath: '/app/resources/runtime/vibeface-core' },
  core,
  businessOpen: false,
  preferences: { openBusinessOnConnect: true },
  profileDir: '/tmp/profile',
  version: '0.0.1-nightly.20261106.3',
  attachOnly: false,
  ...overrides,
})

const ids = (core: CoreState, overrides?: Partial<LauncherStatus>) =>
  toView(status(core, overrides)).actions.map((action) => action.id)

describe('launcher view', () => {
  it('asks for a directory before anything else', () => {
    expect(ids({ kind: 'none' })).toEqual(['choose'])
  })

  it('offers initialization for an empty directory and says what will be created', () => {
    const view = toView(status({ kind: 'uninitialized', dataDir: '/tmp/node' }))
    expect(view.actions[0]?.id).toBe('start')
    expect(view.explain).toContain('身份与配置')
  })

  it('offers both starting and connecting when a directory is idle', () => {
    expect(ids({ kind: 'offline', dataDir: '/tmp/node', deviceId: null })).toEqual(['start', 'attach', 'choose'])
  })

  it('shows the real attempt count while starting and locks the actions', () => {
    const view = toView(status({ kind: 'starting', dataDir: '/tmp/node', attempts: 3 }))
    expect(view.tone).toBe('busy')
    expect(view.explain).toContain('3')
    expect(view.actions.every((action) => action.disabled)).toBe(true)
  })

  it('offers stopping only for a Core this app owns', () => {
    const owned: CoreState = { kind: 'owned', dataDir: '/tmp/node', deviceId: 'a', origin: 'http://127.0.0.1:4791' }
    const attached: CoreState = { ...owned, kind: 'attached' }
    expect(ids(owned)).toContain('stop')
    expect(ids(attached)).not.toContain('stop')
    expect(toView(status(attached)).explain).toContain('退出 App 后也会保持运行')
  })

  it('shows the failure and asks to check the current state', () => {
    const view = toView(status({ kind: 'failed', dataDir: '/tmp/node', message: '端口 4790 被占用，可用 --listen 更换' }))
    expect(view.tone).toBe('bad')
    expect(view.failure).toBe('端口 4790 被占用，可用 --listen 更换')
    expect(view.explain).toBe('可以重新检查当前状态，或调整设置后重试。')
  })

  it('hides every start action in a connection-only run', () => {
    expect(ids({ kind: 'offline', dataDir: '/tmp/node', deviceId: null }, { attachOnly: true })).not.toContain('start')
    expect(ids({ kind: 'uninitialized', dataDir: '/tmp/node' }, { attachOnly: true })).not.toContain('start')
  })

  it('blocks everything when the bundled runtime is missing', () => {
    const view = toView(
      status({ kind: 'none' }, { runtime: { kind: 'missing', corePath: '/absent', detail: '重新安装' } })
    )
    expect(view.actions).toEqual([])
    expect(view.facts).toEqual([['期望路径', '/absent']])
  })

  it('lists the private origin once a Core answers', () => {
    const view = toView(
      status({ kind: 'owned', dataDir: '/tmp/node', deviceId: 'a', origin: 'http://127.0.0.1:4791' })
    )
    expect(view.facts).toContainEqual(['本机入口', 'http://127.0.0.1:4791'])
  })
})
