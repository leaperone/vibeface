import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { Readable } from 'node:stream'
import type { ControlRequest, ControlResponse } from './control-types.js'
import { MAX_CONTROL_PACKET } from './limits.js'

function encodeRequest(request: ControlRequest): Buffer {
  const line = `${JSON.stringify(request)}\n`
  const buf = Buffer.from(line, 'utf8')
  if (buf.length > MAX_CONTROL_PACKET) throw new Error('core control request is too large')
  return buf
}

async function readBoundedStream(stream: Readable, limit: number): Promise<Buffer> {
  const chunks: Buffer[] = []
  let size = 0
  for await (const chunk of stream) {
    const b = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
    size += b.length
    if (size > limit) throw new Error('core control response exceeds size limit')
    chunks.push(b)
  }
  return Buffer.concat(chunks, size)
}

/** Run one `--control` request against the bundled `vibeface-core` (sync child; no token logging). */
export async function runControl(corePath: string, request: ControlRequest): Promise<unknown> {
  const fakeControl = process.env.VIBEFACE_DESKTOP_FAKE_CONTROL?.trim()
  const child = fakeControl
    ? spawn(process.execPath, [fakeControl], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true })
    : spawn(corePath, ['--control'], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true })
  const stdin = child.stdin
  if (!stdin) throw new Error('core control stdin unavailable')
  stdin.end(encodeRequest(request))
  const stdout = child.stdout
  const stderr = child.stderr
  if (!stdout || !stderr) throw new Error('core control stdio unavailable')
  const [out, err, code] = await Promise.all([
    readBoundedStream(stdout, MAX_CONTROL_PACKET),
    readBoundedStream(stderr, MAX_CONTROL_PACKET),
    once(child, 'exit'),
  ])
  const exitCode = code[0] as number | null
  if (exitCode !== 0) {
    const msg = err.toString('utf8').trim() || out.toString('utf8').trim() || `exit ${exitCode}`
    throw new Error(`core control failed: ${msg}`)
  }
  const text = out.toString('utf8').trim()
  const firstLine = text.split('\n')[0] ?? ''
  let parsed: ControlResponse
  try {
    parsed = JSON.parse(firstLine) as ControlResponse
  } catch {
    throw new Error('core control returned invalid JSON')
  }
  if (!parsed.ok) throw new Error(parsed.error ?? 'core control request failed')
  return parsed.result
}
