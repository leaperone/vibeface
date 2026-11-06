import { existsSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { CoreLifecycle, type OwnedExit } from '../src/main/lifecycle.js'
import { serviceFixture } from './fixtures.js'

const settled = async (check: () => boolean, timeoutMs = 5000): Promise<void> => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (check()) return
    await new Promise((resolve) => setTimeout(resolve, 20))
  }
  throw new Error('the owned child never reached the expected state')
}

describe('CoreLifecycle with a real child process', () => {
  let dataDir = ''
  let lifecycle = new CoreLifecycle()

  beforeEach(async () => {
    dataDir = await mkdtemp(join(tmpdir(), 'vibeface-owned-'))
    lifecycle = new CoreLifecycle()
  })

  afterEach(async () => {
    await lifecycle.stopOwned('quit')
    await rm(dataDir, { recursive: true, force: true })
  })

  it('starts a child that only runs with the parent-stdin flag', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    expect(lifecycle.state().kind).toBe('owned')
    await settled(() => existsSync(join(dataDir, 'node.running')))
  })

  it('closing stdin shuts the child down and releases ownership', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(join(dataDir, 'node.running')))
    await lifecycle.stopOwned('quit')
    expect(lifecycle.state().kind).toBe('idle')
    expect(existsSync(join(dataDir, 'node.running'))).toBe(false)
  })

  it('can start again after a clean stop', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(join(dataDir, 'node.running')))
    await lifecycle.stopOwned('quit')
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(join(dataDir, 'node.running')))
    expect(lifecycle.state().kind).toBe('owned')
  })

  it('reports a crashing child and drops back to idle', async () => {
    const exits: OwnedExit[] = []
    lifecycle.onOwnedExit((exit) => exits.push(exit))
    process.env['FIXTURE_CRASH_AFTER_MS'] = '0'
    try {
      lifecycle.startOwned(serviceFixture(dataDir), dataDir)
      await settled(() => lifecycle.state().kind === 'idle')
    } finally {
      delete process.env['FIXTURE_CRASH_AFTER_MS']
    }
    expect(exits.at(-1)?.code).toBe(3)
  })

  it('refuses a second owned child while one runs', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    expect(() => lifecycle.startOwned(serviceFixture(dataDir), dataDir)).toThrow(/already runs/)
  })

  it('refuses to attach while it owns a child, so ownership never changes silently', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    expect(() => lifecycle.attach(dataDir)).toThrow(/stop the Core/)
  })

  it('attaching spawns nothing and stopping an attachment is a no-op', async () => {
    lifecycle.attach(dataDir)
    expect(lifecycle.state()).toEqual({ kind: 'attached', dataDir })
    await lifecycle.stopOwned('quit')
    expect(lifecycle.state().kind).toBe('attached')
  })
})
