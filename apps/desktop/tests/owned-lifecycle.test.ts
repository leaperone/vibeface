import { describe, expect, it, afterEach } from 'vitest'
import { join } from 'node:path'
import { CoreLifecycle } from '../src/main/lifecycle.js'

describe('owned child lifecycle', () => {
  const lc = new CoreLifecycle()
  afterEach(async () => {
    await lc.stopOwned('quit')
  })
  it('spawn and stop owned fake process', async () => {
    const exe = join(import.meta.dirname, 'fake-owned-core.mjs')
    await lc.spawnOwned(process.execPath, '/tmp/vibeface-desktop-owned-test', [exe], {})
    expect(lc.getState().kind).toBe('owned_child')
    await lc.stopOwned('quit')
    expect(lc.getState().kind).toBe('idle')
  })
})
