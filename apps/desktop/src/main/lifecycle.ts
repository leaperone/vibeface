import { spawn, type ChildProcess } from 'node:child_process'
import type { CoreProgram } from './control-client.js'
import { PARENT_STDIN_FLAG } from '../shared/contract.js'

/**
 * A Core the desktop started is a different thing from a Core it merely connected to, so the
 * two are different shapes rather than a pid plus a boolean. Only an owned child is ever
 * stopped: an attached service keeps running when this app quits.
 */
export type Ownership =
  | { kind: 'idle' }
  | { kind: 'attached'; dataDir: string }
  | { kind: 'owned'; dataDir: string; startedAt: number }

export type OwnedExit = { code: number | null; signal: NodeJS.Signals | null }

/** How long a graceful stdin EOF shutdown is awaited before the owned handle is killed. */
const QUIT_GRACE_MS = 20_000
const REPLACE_GRACE_MS = 5_000
const SIGTERM_GRACE_MS = 2_000
const SIGKILL_GRACE_MS = 2_000

type LifecycleTiming = {
  quitGraceMs: number
  replaceGraceMs: number
  sigtermGraceMs: number
  sigkillGraceMs: number
}

const DEFAULT_TIMING: LifecycleTiming = {
  quitGraceMs: QUIT_GRACE_MS,
  replaceGraceMs: REPLACE_GRACE_MS,
  sigtermGraceMs: SIGTERM_GRACE_MS,
  sigkillGraceMs: SIGKILL_GRACE_MS,
}

export class CoreLifecycle {
  private ownership: Ownership = { kind: 'idle' }
  private child: ChildProcess | null = null
  private readonly exitHandlers = new Set<(exit: OwnedExit) => void>()
  private stopping: Promise<void> | null = null

  constructor(private readonly timing: Partial<LifecycleTiming> = {}) {}

  state(): Ownership {
    return this.ownership
  }

  dataDir(): string | null {
    return this.ownership.kind === 'idle' ? null : this.ownership.dataDir
  }

  onOwnedExit(handler: (exit: OwnedExit) => void): void {
    this.exitHandlers.add(handler)
  }

  /** Connection-only: never changes who owns the running service. */
  attach(dataDir: string): void {
    if (this.ownership.kind === 'owned') {
      throw new Error('stop the Core this app started before connecting to another service')
    }
    this.ownership = { kind: 'attached', dataDir }
  }

  detach(): void {
    if (this.ownership.kind === 'attached') this.ownership = { kind: 'idle' }
  }

  /**
   * Start a Core this app owns. The child keeps the parent's stdin pipe open; closing it, or
   * this process dying, is the EOF that asks Core to shut down normally.
   */
  startOwned(program: CoreProgram, dataDir: string): void {
    if (this.ownership.kind === 'owned') throw new Error('this app already runs a Core')
    if (!dataDir) throw new Error('a data directory is required to start a Core')
    const child = spawn(program.command, [...program.args, PARENT_STDIN_FLAG], {
      stdio: ['pipe', 'ignore', 'ignore'],
      windowsHide: true,
      detached: false,
    })
    if (!child.stdin) throw new Error('the owned Core has no stdin to hold open')
    child.stdin.on('error', () => {
      /* the child closing first is reported through 'exit' */
    })
    child.on('error', () => {
      // A spawn failure has no process. An error delivering a signal to a live child is not exit.
      if (child.pid === undefined) this.finish(child, { code: null, signal: null })
    })
    child.on('exit', (code, signal) => {
      this.finish(child, { code, signal })
    })
    this.child = child
    this.ownership = { kind: 'owned', dataDir, startedAt: Date.now() }
  }

  private finish(child: ChildProcess, exit: OwnedExit): void {
    if (this.child !== child) return
    this.child = null
    if (this.ownership.kind === 'owned') this.ownership = { kind: 'idle' }
    for (const handler of this.exitHandlers) handler(exit)
  }

  private grace(reason: 'quit' | 'replace'): number {
    return reason === 'quit'
      ? (this.timing.quitGraceMs ?? DEFAULT_TIMING.quitGraceMs)
      : (this.timing.replaceGraceMs ?? DEFAULT_TIMING.replaceGraceMs)
  }

  private signalGrace(signal: 'SIGTERM' | 'SIGKILL'): number {
    return signal === 'SIGTERM'
      ? (this.timing.sigtermGraceMs ?? DEFAULT_TIMING.sigtermGraceMs)
      : (this.timing.sigkillGraceMs ?? DEFAULT_TIMING.sigkillGraceMs)
  }

  private exited(child: ChildProcess): boolean {
    return this.child !== child || child.exitCode !== null || child.signalCode !== null
  }

  private sendSignal(child: ChildProcess, signal: 'SIGTERM' | 'SIGKILL'): void {
    if (this.exited(child)) return
    try {
      child.kill(signal)
    } catch {
      /* the process can already be gone */
    }
  }

  private async waitForExit(child: ChildProcess, timeoutMs: number): Promise<boolean> {
    if (this.exited(child)) return true
    return await new Promise<boolean>((resolve) => {
      let done = false
      let timer: NodeJS.Timeout | null = null
      const finish = (): void => {
        if (done) return
        done = true
        if (timer) clearTimeout(timer)
        child.off('exit', onExit)
        child.off('close', onClose)
        resolve(this.exited(child))
      }
      const onExit = (): void => finish()
      const onClose = (): void => finish()
      timer = setTimeout(finish, timeoutMs)
      child.once('exit', onExit)
      child.once('close', onClose)
    })
  }

  private async stopOwnedOnce(reason: 'quit' | 'replace'): Promise<void> {
    const child = this.child
    if (!child || this.ownership.kind !== 'owned') return
    try {
      child.stdin?.end()
    } catch {
      /* already closed */
    }

    // Stage 1: EOF graceful shutdown.
    if (await this.waitForExit(child, this.grace(reason))) return

    // Stage 2: SIGTERM, then wait for a confirmed exit.
    this.sendSignal(child, 'SIGTERM')
    if (await this.waitForExit(child, this.signalGrace('SIGTERM'))) return

    // Stage 3: final SIGKILL, then wait for a confirmed exit/close.
    this.sendSignal(child, 'SIGKILL')
    if (await this.waitForExit(child, this.signalGrace('SIGKILL'))) return

    // If exit cannot be confirmed, keep ownership so callers never treat a live child as stopped.
    throw new Error('无法确认本机 Core 已退出；保留 owned 状态以避免误报已停止。')
  }

  /** Close stdin, wait for Core's own shutdown, and only then kill this exact child handle. */
  async stopOwned(reason: 'quit' | 'replace'): Promise<void> {
    if (this.stopping) return this.stopping
    const running = this.stopOwnedOnce(reason)
    this.stopping = running
    try {
      await running
    } finally {
      if (this.stopping === running) this.stopping = null
    }
  }
}
