import { spawn, type ChildProcess } from 'node:child_process'
import type { ControlRequest } from './control-types.js'
import { runControl } from './control-client.js'

/** Desktop-owned Core vs connection-only attachment to an existing service. */
export type CoreLifecycleState =
  | { kind: 'idle' }
  | { kind: 'attached'; dataDir: string }
  | { kind: 'owned_child'; dataDir: string; child: ChildProcess }

export class CoreLifecycle {
  private state: CoreLifecycleState = { kind: 'idle' }

  getState(): Readonly<CoreLifecycleState> {
    return this.state
  }

  attachExisting(dataDir: string) {
    if (this.state.kind === 'owned_child') throw new Error('stop owned Core before attach')
    this.state = { kind: 'attached', dataDir }
  }

  clearAttach() {
    if (this.state.kind === 'attached') this.state = { kind: 'idle' }
  }

  dataDir(): string | null {
    switch (this.state.kind) {
      case 'attached':
      case 'owned_child':
        return this.state.dataDir
      default:
        return null
    }
  }

  async spawnOwned(
    corePath: string,
    dataDir: string,
    args: string[],
    env: NodeJS.ProcessEnv
  ): Promise<void> {
    if (!dataDir) throw new Error('--data-dir is required for owned Core')

    if (this.state.kind === 'owned_child') await this.stopOwned('replace')

    const child = spawn(corePath, args, {
      env: { ...process.env, ...env },
      stdio: ['pipe', 'ignore', 'ignore'],
      windowsHide: true,
    })
    if (!child.stdin) throw new Error('owned Core stdin unavailable')
    // Keep stdin open until quit; parent closes it on quit for graceful shutdown (Core flag TBD).
    child.on('error', () => {
      /* surfaced via process 'exit' */
    })
    child.on('exit', () => {
      if (this.state.kind === 'owned_child' && this.state.child === child) {
        this.state = { kind: 'idle' }
      }
    })
    this.state = { kind: 'owned_child', dataDir, child }
  }

  async inspect(corePath: string): Promise<unknown> {
    const dir = this.dataDir()
    if (!dir) throw new Error('select a data directory first')
    const request: ControlRequest = { command: 'inspect', data_dir: dir }
    return runControl(corePath, request)
  }

  async accessUrl(corePath: string, json: boolean): Promise<unknown> {
    const dir = this.dataDir()
    if (!dir) throw new Error('select a data directory first')
    return runControl(corePath, { command: 'access', data_dir: dir, json })
  }

  async stopOwned(mode: 'quit' | 'replace'): Promise<void> {
    if (this.state.kind !== 'owned_child') return
    const { child } = this.state
    try {
      child.stdin?.end()
    } catch {
      /* ignore */
    }
    const deadline = Date.now() + (mode === 'quit' ? 20000 : 5000)
    while (Date.now() < deadline && child.exitCode == null && child.signalCode == null) {
      await new Promise((r) => setTimeout(r, 80))
    }
    if (child.exitCode == null && child.signalCode == null) {
      try {
        child.kill('SIGTERM')
      } catch {
        /* ignore */
      }
    }
    if (this.state.kind === 'owned_child' && this.state.child === child) {
      this.state = { kind: 'idle' }
    }
  }
}
