import { existsSync } from 'node:fs'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { CoreSession, type Programs } from '../src/main/session.js'
import { CONTROL_FIXTURE, serviceFixture } from './fixtures.js'

const programs: Programs = {
  runtime: () => ({ kind: 'ready', corePath: '/fixture/vibeface-core' }),
  control: () => CONTROL_FIXTURE,
  service: serviceFixture,
}

const waitFor = async (check: () => boolean, timeoutMs = 8000): Promise<void> => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (check()) return
    await new Promise((resolve) => setTimeout(resolve, 20))
  }
  throw new Error('the session never reached the expected state')
}

describe('CoreSession against protocol fixtures', () => {
  let root = ''
  let profileDir = ''
  let dataDir = ''
  let session: CoreSession

  const open = (attachOnly = false): CoreSession =>
    new CoreSession(programs, { profileDir, attachOnly, onChange: () => {}, startTimeoutMs: 8000 })

  beforeEach(async () => {
    root = await mkdtemp(join(tmpdir(), 'vibeface-session-'))
    profileDir = join(root, 'profile')
    dataDir = join(root, 'node')
    session = open()
  })

  afterEach(async () => {
    await session.shutdown()
    await rm(root, { recursive: true, force: true })
  })

  it('reports an unselected directory as uninitialized without writing to it', async () => {
    await session.useDataDir(dataDir)
    expect(session.state()).toEqual({ kind: 'uninitialized', dataDir })
    expect(existsSync(join(dataDir, 'config.json'))).toBe(false)
  })

  it('starts a new desktop in its own profile only when the user chooses to start', async () => {
    await session.restore()
    expect(session.state()).toEqual({ kind: 'none' })
    expect(existsSync(join(profileDir, 'node'))).toBe(false)
    await session.startOwned()
    expect(session.state().kind).toBe('owned')
    expect(session.dataDir()).toBe(join(profileDir, 'node'))
    expect(existsSync(join(profileDir, 'node/config.json'))).toBe(true)
  })

  it('initializes, starts and then serves the Core private origin', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    expect(session.state().kind).toBe('owned')
    expect(session.ownsCore()).toBe(true)
    const target = await session.businessTarget()
    expect(target.startsWith('http://127.0.0.1:4791/')).toBe(true)
    expect(target).toContain('#local=')
  })

  it('stopping an owned Core leaves the directory offline, and it can start again', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    await session.stopOwned()
    expect(session.state().kind).toBe('offline')
    expect(session.ownsCore()).toBe(false)
    await session.startOwned()
    expect(session.state().kind).toBe('owned')
  })

  it('refuses to start a second Core over a directory that already serves one', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    const other = open()
    await other.useDataDir(dataDir)
    expect(other.state().kind).toBe('attached')
    await expect(other.startOwned()).rejects.toThrow(/已经有 Core 在运行/)
    await other.shutdown()
    expect(session.state().kind).toBe('owned')
  })

  it('quitting a connection-only session never stops the running service', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    const viewer = open()
    await viewer.useDataDir(dataDir)
    await viewer.attachExisting()
    expect(viewer.state().kind).toBe('attached')
    await viewer.shutdown()
    expect(existsSync(join(dataDir, 'node.running'))).toBe(true)
  })

  it('refuses to connect when nothing is running there', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    await session.stopOwned()
    await expect(session.attachExisting()).rejects.toThrow(/没有正在运行的 Core/)
  })

  it('an attach-only run never starts a Core', async () => {
    const viewer = open(true)
    await viewer.useDataDir(dataDir)
    await expect(viewer.startOwned()).rejects.toThrow(/仅连接模式/)
    expect(existsSync(join(dataDir, 'config.json'))).toBe(false)
  })

  it('surfaces a crashing child as a failure instead of a running Core', async () => {
    process.env['FIXTURE_CRASH_AFTER_MS'] = '0'
    try {
      const crashing = new CoreSession(programs, {
        profileDir,
        attachOnly: false,
        onChange: () => {},
        startTimeoutMs: 1500,
      })
      await crashing.useDataDir(dataDir)
      await expect(crashing.startOwned()).rejects.toThrow()
      await waitFor(() => crashing.state().kind === 'failed')
      await crashing.shutdown()
    } finally {
      delete process.env['FIXTURE_CRASH_AFTER_MS']
    }
  })

  it('remembers the last directory across sessions but starts nothing on its own', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    await session.stopOwned()
    const next = open()
    await next.restore()
    expect(next.state()).toMatchObject({ kind: 'offline', dataDir })
    expect(next.ownsCore()).toBe(false)
  })

  it('refuses a relative data directory', async () => {
    await expect(session.useDataDir('relative/node')).rejects.toThrow(/绝对路径/)
  })

  it('never writes a token or an access URL into its settings file', async () => {
    await session.useDataDir(dataDir)
    await session.startOwned()
    await session.businessTarget()
    const settings: unknown = JSON.parse(await readFile(join(profileDir, 'desktop-settings.json'), 'utf8'))
    expect(settings).toEqual({ lastDataDir: dataDir, preferences: { openBusinessOnConnect: true } })
  })

  it('reports a missing runtime instead of guessing a program', async () => {
    const broken = new CoreSession(
      { ...programs, runtime: () => ({ kind: 'missing', corePath: '/absent/vibeface-core', detail: 'gone' }) },
      { profileDir, attachOnly: false, onChange: () => {}, startTimeoutMs: 1000 }
    )
    await writeFile(join(root, 'marker'), '')
    await broken.useDataDir(dataDir)
    expect(broken.state()).toMatchObject({ kind: 'failed' })
  })
})
