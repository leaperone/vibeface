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
const POLL_MS = 50

export class CoreLifecycle {
  private ownership: Ownership = { kind: 'idle' }
  private child: ChildProcess | null = null
  private readonly exitHandlers = new Set<(exit: OwnedExit) => void>()

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
      this.finish(child, { code: null, signal: null })
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

  /** Close stdin, wait for Core's own shutdown, and only then kill this exact child handle. */
  async stopOwned(reason: 'quit' | 'replace'): Promise<void> {
    const child = this.child
    if (!child || this.ownership.kind !== 'owned') return
    try {
      child.stdin?.end()
    } catch {
      /* already closed */
    }
    const deadline = Date.now() + (reason === 'quit' ? QUIT_GRACE_MS : REPLACE_GRACE_MS)
    while (Date.now() < deadline && child.exitCode === null && child.signalCode === null) {
      await new Promise((resolve) => setTimeout(resolve, POLL_MS))
    }
    if (child.exitCode === null && child.signalCode === null) {
      try {
        child.kill('SIGTERM')
      } catch {
        /* the handle is gone */
      }
    }
    this.finish(child, { code: child.exitCode, signal: child.signalCode })
  }
}
