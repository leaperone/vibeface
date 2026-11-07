import { spawn, type ChildProcess } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CoreLifecycle, type OwnedExit } from '../src/main/lifecycle.js'
import { serviceFixture } from './fixtures.js'

vi.mock('node:child_process', async (original) => {
  const actual = await original<typeof import('node:child_process')>()
  return { ...actual, spawn: vi.fn(actual.spawn) }
})

const settled = async (check: () => boolean, timeoutMs = 5000): Promise<void> => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (check()) return
    await new Promise((resolve) => setTimeout(resolve, 20))
  }
  throw new Error('the owned child never reached the expected state')
}

const markerPath = (dataDir: string): string => join(dataDir, 'node.running')

const runningPid = async (dataDir: string): Promise<number> => {
  const raw = await readFile(markerPath(dataDir), 'utf8')
  return Number(raw.trim())
}

const pidAlive = (pid: number): boolean => {
  if (!Number.isFinite(pid) || pid <= 0) return false
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

describe('CoreLifecycle with a real child process', () => {
  let dataDir = ''
  let lifecycle = new CoreLifecycle()

  beforeEach(async () => {
    vi.mocked(spawn).mockClear()
    dataDir = await mkdtemp(join(tmpdir(), 'vibeface-owned-'))
    lifecycle = new CoreLifecycle()
    delete process.env['FIXTURE_CRASH_AFTER_MS']
    delete process.env['FIXTURE_IGNORE_PARENT_EOF']
    delete process.env['FIXTURE_IGNORE_SIGTERM']
  })

  afterEach(async () => {
    delete process.env['FIXTURE_CRASH_AFTER_MS']
    delete process.env['FIXTURE_IGNORE_PARENT_EOF']
    delete process.env['FIXTURE_IGNORE_SIGTERM']
    await lifecycle.stopOwned('quit').catch(() => {
      /* tests that force unknown outcomes clean up the child explicitly */
    })
    for (const call of vi.mocked(spawn).mock.results) {
      if (call.type !== 'return') continue
      const child = call.value
      if (child.pid && pidAlive(child.pid)) {
        child.kill('SIGKILL')
        await settled(() => !child.pid || !pidAlive(child.pid))
      }
    }
    await rm(dataDir, { recursive: true, force: true })
  })

  it('starts a child that only runs with the parent-stdin flag', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    expect(lifecycle.state().kind).toBe('owned')
    await settled(() => existsSync(markerPath(dataDir)))
  })

  it('closing stdin shuts the child down and releases ownership', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    await lifecycle.stopOwned('quit')
    expect(lifecycle.state().kind).toBe('idle')
    expect(existsSync(markerPath(dataDir))).toBe(false)
  })

  it('can start again after a clean stop', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    await lifecycle.stopOwned('quit')
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    expect(lifecycle.state().kind).toBe('owned')
  })

  it('can stop a second owned process after restarting it', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    await lifecycle.stopOwned('quit')
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    const secondPid = await runningPid(dataDir)
    await lifecycle.stopOwned('quit')
    expect(lifecycle.state().kind).toBe('idle')
    expect(pidAlive(secondPid)).toBe(false)
  })

  it('keeps a running child owned when a process operation reports an error', async () => {
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    const call = vi.mocked(spawn).mock.results.at(-1)
    if (!call || call.type !== 'return') throw new Error('expected a real spawned child')
    const child = call.value
    const pid = await runningPid(dataDir)
    child.emit('error', new Error('fixture signal delivery failed'))
    expect(lifecycle.state().kind).toBe('owned')
    expect(pidAlive(pid)).toBe(true)
    await lifecycle.stopOwned('quit')
    expect(pidAlive(pid)).toBe(false)
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

  it('waits for a confirmed exit before clearing owned state during fallback stop', async () => {
    lifecycle = new CoreLifecycle({ replaceGraceMs: 120, sigtermGraceMs: 900, sigkillGraceMs: 300 })

    process.env['FIXTURE_IGNORE_PARENT_EOF'] = '1'
    process.env['FIXTURE_IGNORE_SIGTERM'] = '1'

    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    const pid = await runningPid(dataDir)
    const call = vi.mocked(spawn).mock.results.at(-1)
    if (!call || call.type !== 'return') throw new Error('expected a real spawned child')
    const child = call.value
    const realKill = child.kill.bind(child)
    let requestedSignal: (() => void) | undefined
    const signalRequested = new Promise<void>((resolve) => {
      requestedSignal = resolve
    })
    const kill = vi.spyOn(child, 'kill').mockImplementation((signal) => {
      if (signal === 'SIGTERM') {
        requestedSignal?.()
        return true
      }
      return realKill(signal)
    })

    const stopping = lifecycle.stopOwned('replace')
    await signalRequested

    expect(lifecycle.state().kind).toBe('owned')
    expect(pidAlive(pid)).toBe(true)

    kill.mockRestore()
    realKill('SIGTERM')
    await stopping
    expect(lifecycle.state().kind).toBe('idle')
    expect(pidAlive(pid)).toBe(false)
  })

  it('retains owned state and errors when exit cannot be confirmed', async () => {
    lifecycle = new CoreLifecycle({ replaceGraceMs: 80, sigtermGraceMs: 80, sigkillGraceMs: 80 })
    process.env['FIXTURE_IGNORE_PARENT_EOF'] = '1'
    if (process.platform !== 'win32') process.env['FIXTURE_IGNORE_SIGTERM'] = '1'

    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))

    const holder = lifecycle as unknown as { child: ChildProcess | null }
    const child = holder.child
    expect(child).not.toBeNull()
    if (!child) return

    const realKill = child.kill.bind(child)
    ;(child as ChildProcess & { kill: (signal?: NodeJS.Signals | number) => boolean }).kill = () => false
    try {
      await expect(lifecycle.stopOwned('replace')).rejects.toThrow(/无法确认后台进程已退出/)
      expect(lifecycle.state().kind).toBe('owned')
    } finally {
      ;(child as ChildProcess & { kill: (signal?: NodeJS.Signals | number) => boolean }).kill = realKill
      realKill('SIGKILL')
      await settled(() => lifecycle.state().kind === 'idle')
    }
  })

  it('treats parallel stop requests as idempotent', async () => {
    lifecycle = new CoreLifecycle({ replaceGraceMs: 120, sigtermGraceMs: 120, sigkillGraceMs: 120 })
    lifecycle.startOwned(serviceFixture(dataDir), dataDir)
    await settled(() => existsSync(markerPath(dataDir)))
    await Promise.all([lifecycle.stopOwned('replace'), lifecycle.stopOwned('replace')])
    expect(lifecycle.state().kind).toBe('idle')
  })
})
