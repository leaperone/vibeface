import { spawn } from 'node:child_process'
import { once } from 'node:events'
import type { Readable } from 'node:stream'
import { MAX_CONTROL_PACKET } from './limits.js'
import { type ControlRequest, parseControlResponse } from './control-types.js'

/** The exact program the desktop is allowed to run, resolved once from the bundled runtime. */
export type CoreProgram = { command: string; args: readonly string[] }

const CONTROL_TIMEOUT_MS = 30_000
const CLOSE_TIMEOUT_MS = 2_000

function encodeRequest(request: ControlRequest): Buffer {
  const line = Buffer.from(`${JSON.stringify(request)}\n`, 'utf8')
  if (line.length > MAX_CONTROL_PACKET) throw new Error('core control request is too large')
  return line
}

async function readBounded(stream: Readable, limit: number): Promise<string> {
  const chunks: Buffer[] = []
  let size = 0
  for await (const chunk of stream) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), 'utf8')
    size += bytes.length
    if (size > limit) throw new Error('core control response exceeds its size limit')
    chunks.push(bytes)
  }
  return Buffer.concat(chunks, size).toString('utf8')
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, onTimeout: () => void, message: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let settled = false
    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      onTimeout()
      reject(new Error(message))
    }, timeoutMs)
    void promise.then(
      (value) => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        resolve(value)
      },
      (error) => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        reject(error)
      }
    )
  })
}

/**
 * Run one bounded control request. Core writes a single JSON line and exits; its stderr is
 * read only to bound it, and never copied into an error message or a log, because an
 * `access` reply and its diagnostics can carry a single-use local token.
 */
export async function runControl(program: CoreProgram, request: ControlRequest): Promise<unknown> {
  const child = spawn(program.command, [...program.args], {
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  })
  const { stdin, stdout, stderr } = child
  if (!stdin || !stdout || !stderr) throw new Error('core control stdio is unavailable')

  stdin.on('error', () => {
    /* a core that exits before reading the request surfaces through 'error' or an empty reply */
  })
  const failure = once(child, 'error').then(([error]: unknown[]) => {
    throw error instanceof Error ? error : new Error('core control could not start')
  })
  void failure.catch(() => {
    /* races with success; swallowed to avoid unhandled rejections */
  })
  const closePromise = once(child, 'close').then(([code, signal]: unknown[]) => ({
    code: typeof code === 'number' || code === null ? code : null,
    signal: typeof signal === 'string' || signal === null ? (signal as NodeJS.Signals | null) : null,
  }))
  void closePromise.catch(() => {
    /* consumed by the failure race when the process cannot spawn */
  })
  stdin.end(encodeRequest(request))
  const streams = withTimeout(
    Promise.all([readBounded(stdout, MAX_CONTROL_PACKET), readBounded(stderr, MAX_CONTROL_PACKET)]),
    CONTROL_TIMEOUT_MS,
    () => {
      try {
        child.kill('SIGKILL')
      } catch {
        /* already gone */
      }
    },
    'core control timed out before producing a complete bounded response'
  )
  void streams.catch(() => {
    /* consumed by the failure race on spawn errors */
  })
  const [out] = await Promise.race([streams, failure])
  const closeWait = withTimeout(
    closePromise,
    CLOSE_TIMEOUT_MS,
    () => {
      try {
        child.kill('SIGKILL')
      } catch {
        /* already gone */
      }
    },
    'core control closed stdio without a confirmed process exit; outcome is unknown'
  )
  void closeWait.catch(() => {
    /* consumed by the failure race */
  })
  const close = await Promise.race([closeWait, failure])
  if (close.code !== 0 || close.signal !== null) {
    const reason = close.signal ? `signal ${close.signal}` : `code ${String(close.code)}`
    throw new Error(`core control exited with ${reason}`)
  }
  const line = out.split('\n', 1)[0]?.trim() ?? ''
  if (!line) throw new Error('core control wrote no response')
  let json: unknown
  try {
    json = JSON.parse(line)
  } catch {
    throw new Error('core control returned invalid JSON')
  }
  const response = parseControlResponse(json)
  if (!response.ok) throw new Error(response.error)
  return response.result
}
