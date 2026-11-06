import { spawn } from 'node:child_process'
import { once } from 'node:events'
import type { Readable } from 'node:stream'
import { MAX_CONTROL_PACKET } from './limits.js'
import { type ControlRequest, parseControlResponse } from './control-types.js'

/** The exact program the desktop is allowed to run, resolved once from the bundled runtime. */
export type CoreProgram = { command: string; args: readonly string[] }

const CONTROL_TIMEOUT_MS = 30_000

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
  const timer = setTimeout(() => child.kill('SIGKILL'), CONTROL_TIMEOUT_MS)
  try {
    const failure = once(child, 'error').then(([error]: unknown[]) => {
      throw error instanceof Error ? error : new Error('core control could not start')
    })
    stdin.end(encodeRequest(request))
    const [out] = await Promise.race([
      Promise.all([
        readBounded(stdout, MAX_CONTROL_PACKET),
        readBounded(stderr, MAX_CONTROL_PACKET),
        once(child, 'close'),
      ]),
      failure,
    ])
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
  } finally {
    clearTimeout(timer)
  }
}
