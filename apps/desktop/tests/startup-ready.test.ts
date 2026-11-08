import { EventEmitter } from 'node:events'
import { describe, expect, it, vi } from 'vitest'
import { reloadClientContents } from '../src/main/client-navigation.js'

const contents = () => Object.assign(new EventEmitter(), { isDestroyed: () => false, reload: vi.fn() })

describe('client reload readiness', () => {
  it('waits for the main frame even when reload has not started loading yet', async () => {
    const live = contents()
    let resolved = false
    const pending = reloadClientContents(live, 500).then(() => { resolved = true })
    expect(live.reload).toHaveBeenCalledTimes(1)
    await Promise.resolve()
    expect(resolved).toBe(false)
    live.emit('did-fail-load', {}, -3, 'child aborted', 'https://example.test', false)
    await Promise.resolve()
    expect(resolved).toBe(false)
    live.emit('did-finish-load')
    await pending
    expect(live.listenerCount('did-fail-load')).toBe(0)
  })

  it('observes a load that completes inside the reload call', async () => {
    const live = contents()
    live.reload.mockImplementation(() => { live.emit('did-finish-load') })
    await expect(reloadClientContents(live)).resolves.toBeUndefined()
  })

  it('reports main-frame failure and removes all navigation listeners', async () => {
    const live = contents()
    const pending = reloadClientContents(live, 500)
    live.emit('did-fail-load', {}, -3, 'aborted', 'vibeface-app://app/', true)
    await expect(pending).rejects.toThrow(/加载失败/)
    expect(live.listenerCount('did-finish-load')).toBe(0)
    expect(live.listenerCount('destroyed')).toBe(0)
  })

  it('reports window destruction instead of waiting for a load that cannot finish', async () => {
    const live = contents()
    const pending = reloadClientContents(live, 500)
    live.emit('destroyed')
    await expect(pending).rejects.toThrow(/窗口已关闭/)
    expect(live.listenerCount('did-finish-load')).toBe(0)
  })
})
