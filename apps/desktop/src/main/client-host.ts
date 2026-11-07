import { randomUUID } from 'node:crypto'
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process'
import { realpath, readFile, stat } from 'node:fs/promises'
import { extname, isAbsolute, relative, resolve, sep } from 'node:path'
import { isIP } from 'node:net'
import { createInterface } from 'node:readline'
import { request as httpRequest, type IncomingMessage } from 'node:http'
import { ipcMain, protocol, type IpcMainEvent, type IpcMainInvokeEvent, type WebContents } from 'electron'
import { CLIENT_HOST, CLIENT_IPC, CLIENT_ORIGIN, CLIENT_SCHEME, CLIENT_URL, type ClientFrame, type ClientLocalEntry, type ClientLocalLaunch, type ClientLocalSetup } from '../shared/client-contract.js'
import { businessUrl, parsePrivateOrigin } from './business-url.js'
import type { Access, Inspect } from './control-types.js'
import WebSocket from 'ws'

export const CLIENT_SCHEME_DESCRIPTOR = {
  scheme: CLIENT_SCHEME,
  privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: false },
} as const

export type SelectedLocalNode = { inspect: Inspect; access: Access }
export type ClientHostOptions = {
  /** The signed runtime's complete `web` asset directory. */
  runtimeRoot: string
  /** Already verified current-user selection. It may return null and must not initialize a Node. */
  resolveSelectedNode(): Promise<SelectedLocalNode | null>
  /** Absolute bundled Core path, invoked only for one direct LAN client stream. */
  clientBridgeProgram(): string
  /** The root owns window creation and supplies only its trusted client WebContents. */
  getClientContents(): WebContents | null
  showNodePanel(): void | Promise<void>
}

const DEVICE_ID = /^[a-f0-9]{64}$/
const HEX_TOKEN = /^[a-f0-9]{64}$/
const MAX_HTTP_BODY = 1024 * 1024
const MAX_WS_MESSAGE = 65535
const MAX_LAN_FRAME = 1024 * 1024
const MAX_QUEUED_FRAMES = 16
const MAX_LAN_ADDRESSES = 16
const MIME = new Map<string, string>([
  ['.html', 'text/html; charset=utf-8'], ['.js', 'text/javascript; charset=utf-8'], ['.mjs', 'text/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'], ['.json', 'application/json; charset=utf-8'], ['.wasm', 'application/wasm'],
  ['.svg', 'image/svg+xml'], ['.png', 'image/png'], ['.jpg', 'image/jpeg'], ['.jpeg', 'image/jpeg'], ['.webp', 'image/webp'],
  ['.ico', 'image/x-icon'], ['.woff', 'font/woff'], ['.woff2', 'font/woff2'], ['.ttf', 'font/ttf'], ['.otf', 'font/otf'],
])

type LocalTarget = { nodeId: string; host: string; port: number; origin: string }
type HttpReply = { status: number; headers: IncomingMessage['headers']; body: Buffer }
type FrameCarrier = { send(bytes: Uint8Array): Promise<void>; receive(): Promise<ClientFrame | null>; close(): void }
type LocalSocket = { nodeId: string; contents: WebContents; socket: FrameCarrier; receiving: boolean; sending: number }

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function sameClientOrigin(raw: string): boolean {
  try {
    const url = new URL(raw)
    return url.protocol === `${CLIENT_SCHEME}:` && url.host === CLIENT_HOST && !url.username && !url.password && !url.port
  } catch { return false }
}

function validateRenderer(event: IpcMainInvokeEvent | IpcMainEvent, getContents: () => WebContents | null): WebContents {
  const trusted = getContents()
  const frame = event.senderFrame
  if (!trusted || event.sender !== trusted || !frame || frame !== trusted.mainFrame || frame.parent !== null || !sameClientOrigin(frame.url)) {
    throw new Error('Untrusted client window')
  }
  return trusted
}

function selectedTarget(value: SelectedLocalNode | null): LocalTarget | null {
  if (value === null) return null
  const { inspect, access } = value
  if (!DEVICE_ID.test(inspect.deviceId) || inspect.status !== 'running' || !inspect.webEnabled || inspect.publicEntry
      || access.kind !== 'local' || access.deviceId !== inspect.deviceId) return null
  const privateOrigin = parsePrivateOrigin(inspect.localListen)
  try {
    const canonical = new URL(businessUrl(access, inspect))
    if (canonical.origin !== privateOrigin.origin) return null
  } catch { return null }
  // The native broker uses local_listen only; access URLs and their fragments never cross IPC.
  const hostHeader = privateOrigin.host.includes(':') ? `[${privateOrigin.host.replace(/^\[|\]$/g, '')}]` : privateOrigin.host
  return { nodeId: inspect.deviceId, host: privateOrigin.host.replace(/^\[|\]$/g, ''), port: privateOrigin.port, origin: privateOrigin.origin }
}

function readBoundedBody(response: IncomingMessage): Promise<Buffer> {
  return new Promise((resolveBody, reject) => {
    const chunks: Buffer[] = []
    let size = 0
    response.on('data', (chunk: Buffer | string) => {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
      size += bytes.length
      if (size > MAX_HTTP_BODY) { response.destroy(new Error('Local response too large')); return }
      chunks.push(bytes)
    })
    response.on('end', () => resolveBody(Buffer.concat(chunks, size)))
    response.on('error', reject)
  })
}

async function localHttp(target: LocalTarget, request: { method: 'GET' | 'POST'; path: '/api/local/setup' | '/api/local/info' | '/api/local/exchange'; body?: unknown; cookie?: string; signal?: AbortSignal }): Promise<HttpReply> {
  if (request.signal?.aborted) throw new Error('Local request cancelled')
  const body = request.body === undefined ? undefined : Buffer.from(JSON.stringify(request.body), 'utf8')
  return new Promise((resolveReply, reject) => {
    let settled = false
    const finishError = (error: Error) => { if (settled) return; settled = true; reject(error) }
    const req = httpRequest({
      hostname: target.host,
      port: target.port,
      method: request.method,
      path: request.path,
      headers: {
        Host: `${target.host.includes(':') ? `[${target.host}]` : target.host}:${target.port}`,
        Origin: target.origin,
        Accept: 'application/json',
        ...(body ? { 'Content-Type': 'application/json', 'Content-Length': String(body.length) } : {}),
        ...(request.cookie ? { Cookie: request.cookie } : {}),
      },
      setHost: false,
      agent: false,
      timeout: 5000,
    }, (response) => {
      void readBoundedBody(response).then((responseBody) => {
        if (settled) return
        settled = true
        resolveReply({ status: response.statusCode ?? 0, headers: response.headers, body: responseBody })
      }, finishError)
    })
    const abort = () => req.destroy(new Error('Local request cancelled'))
    request.signal?.addEventListener('abort', abort, { once: true })
    req.on('error', finishError)
    req.on('timeout', () => req.destroy(new Error('Local request timed out')))
    req.on('close', () => request.signal?.removeEventListener('abort', abort))
    if (body) req.end(body); else req.end()
  })
}

function jsonBody(reply: HttpReply): Record<string, unknown> {
  if (reply.body.length === 0) throw new Error('Invalid local response')
  let value: unknown
  try { value = JSON.parse(reply.body.toString('utf8')) } catch { throw new Error('Invalid local response') }
  if (!isRecord(value)) throw new Error('Invalid local response')
  return value
}

function localEntry(value: unknown, target: LocalTarget, requireLocal = false): ClientLocalEntry {
  if (!isRecord(value) || value.device_id !== target.nodeId || typeof value.address !== 'string' || value.address !== target.origin
      || (requireLocal && value.local !== true)) throw new Error('Local Node identity changed')
    return { kind: 'local', address: target.origin, device_id: target.nodeId }
}

function parseSetup(value: unknown, target: LocalTarget): ClientLocalSetup {
  if (!isRecord(value)) throw new Error('Invalid local setup response')
  if (value.kind === 'uninitialized' && isRecord(value.invitation)) {
    const invitation = value.invitation
    if (invitation.url !== target.origin || invitation.device_id !== target.nodeId || typeof invitation.capability !== 'string' || !HEX_TOKEN.test(invitation.capability)) {
      throw new Error('Local setup belongs to another Node')
    }
    return { kind: 'uninitialized', entry: { kind: 'gateway', address: target.origin, device_id: target.nodeId }, capability: invitation.capability }
  }
  if (value.kind === 'initialized') {
    const rawEntry = value.entry
    if (!isRecord(rawEntry) || rawEntry.device_id !== target.nodeId || rawEntry.address !== target.origin) throw new Error('Local Node identity changed')
    const entry: ClientLocalSetup['entry'] = { kind: 'gateway', address: target.origin, device_id: target.nodeId }
    if (typeof value.removed !== 'boolean') throw new Error('Invalid local setup response')
    const result: Extract<ClientLocalSetup, { kind: 'initialized' }> = { kind: 'initialized', entry, removed: value.removed }
    if (value.local_launch !== undefined) {
      if (!isRecord(value.local_launch) || value.local_launch.device_id !== target.nodeId || typeof value.local_launch.url !== 'string'
          || typeof value.local_launch.token !== 'string' || !HEX_TOKEN.test(value.local_launch.token)) throw new Error('Invalid local launch')
      const launch = new URL(value.local_launch.url)
      if (launch.protocol !== 'http:' || launch.origin !== target.origin || launch.username || launch.password || launch.pathname !== '/' || launch.search || launch.hash) throw new Error('Invalid local launch')
      result.local_launch = { url: target.origin, device_id: target.nodeId, token: value.local_launch.token }
    }
    return result
  }
  throw new Error('Invalid local setup response')
}

function parseCsp(receipt: unknown): string {
  if (!isRecord(receipt) || receipt.version !== 1 || receipt.component !== 'web' || typeof receipt.content_security_policy !== 'string') {
    throw new Error('The signed Web build has no valid CSP receipt')
  }
  const csp = receipt.content_security_policy.trim()
  if (!csp || csp.length > 8192 || /[\r\n]/.test(csp) || !csp.includes("default-src 'self'") || !csp.includes("script-src 'self'")) {
    throw new Error('The signed Web build has an invalid CSP')
  }
  return csp.replaceAll("'self'", CLIENT_ORIGIN)
}

function pathForAsset(rawPath: string): string | null {
  let decoded: string
  try { decoded = decodeURIComponent(rawPath) } catch { return null }
  if (decoded.includes('\0') || decoded.includes('\\') || decoded.split('/').some(part => part === '.' || part === '..')) return null
  const relativePath = decoded.replace(/^\/+/, '') || 'index.html'
  const segments = relativePath.split('/')
  if (segments.some(part => !part || part.startsWith('.')) || ['_headers', '_redirects', '_shell.html'].includes(relativePath)) return null
  return relativePath
}

/** Bounded WebSocket channel for Core's fixed local client endpoints. */
class ClientWebSocket implements FrameCarrier {
  private readonly socket: WebSocket
  private readonly frames: ClientFrame[] = []
  private readonly waiters: { resolve(frame: ClientFrame | null): void; reject(error: Error): void }[] = []
  private readonly opened: Promise<void>
  private resolveOpened!: () => void
  private rejectOpened!: (error: Error) => void
  private closed = false
  private failure: Error | null = null

  private constructor(target: LocalTarget, path: '/api/local/ws' | '/api/client', cookie: string | null, timeoutMs: number) {
    const endpoint = new URL(path, target.origin)
    endpoint.protocol = 'ws:'
    const authority = target.host.includes(':') ? `[${target.host}]` : target.host
    this.socket = new WebSocket(endpoint.href, {
      headers: {
        Host: `${authority}:${target.port}`,
        Origin: target.origin,
        ...(cookie ? { Cookie: cookie } : {}),
      },
      followRedirects: false,
      handshakeTimeout: timeoutMs,
      maxPayload: MAX_WS_MESSAGE,
      perMessageDeflate: false,
    })
    this.opened = new Promise((resolveOpen, rejectOpen) => { this.resolveOpened = resolveOpen; this.rejectOpened = rejectOpen })
    this.socket.on('open', () => this.resolveOpened())
    this.socket.on('message', (raw, isBinary) => {
      const data = Buffer.isBuffer(raw) ? raw : Array.isArray(raw) ? Buffer.concat(raw) : Buffer.from(raw)
      const frame: ClientFrame = { text: !isBinary, data: new Uint8Array(data) }
      const waiter = this.waiters.shift()
      if (waiter) waiter.resolve(frame)
      else if (this.frames.length < MAX_QUEUED_FRAMES) this.frames.push(frame)
      else this.fail(new Error('Local connection backpressure limit reached'))
    })
    this.socket.on('error', (error) => this.fail(error))
    this.socket.on('close', () => this.fail(new Error('Local connection closed')))
  }

  static async open(target: LocalTarget, path: '/api/local/ws' | '/api/client', cookie: string | null, timeoutMs: number, connectTarget?: string): Promise<ClientWebSocket> {
    const channel = new ClientWebSocket(target, path, cookie, timeoutMs)
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      await Promise.race([
        channel.opened,
        new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error('Local connection timed out')), timeoutMs) }),
      ])
      if (connectTarget) await channel.sendText(JSON.stringify({ type: 'connect', target: connectTarget }))
      return channel
    } catch (error) {
      channel.close()
      throw error
    } finally { if (timer) clearTimeout(timer) }
  }

  private fail(error: Error): void {
    if (this.closed) return
    this.failure = error
    this.closed = true
    this.rejectOpened(error)
    this.socket.terminate()
    this.frames.length = 0
    for (const waiter of this.waiters.splice(0)) waiter.reject(error)
  }

  private sendFrame(data: Uint8Array, binary: boolean): Promise<void> {
    if (this.closed || this.socket.readyState !== WebSocket.OPEN) return Promise.reject(this.failure ?? new Error('Local connection closed'))
    if (data.byteLength > MAX_WS_MESSAGE) return Promise.reject(new Error('Local frame too large'))
    return new Promise((resolveSend, rejectSend) => {
      const payload = binary ? Buffer.from(data) : Buffer.from(data).toString('utf8')
      this.socket.send(payload, { binary, compress: false }, (error) => error ? rejectSend(error) : resolveSend())
    })
  }

  private sendText(value: string): Promise<void> {
    return this.sendFrame(new TextEncoder().encode(value), false)
  }

  send(bytes: Uint8Array): Promise<void> { return this.sendFrame(bytes, true) }

  receive(): Promise<ClientFrame | null> {
    if (this.frames.length) return Promise.resolve(this.frames.shift() ?? null)
    if (this.failure) return Promise.reject(this.failure)
    if (this.closed) return Promise.resolve(null)
    if (this.waiters.length > 0) return Promise.reject(new Error('A frame receive is already pending'))
    return new Promise((resolveFrame, rejectFrame) => this.waiters.push({ resolve: resolveFrame, reject: rejectFrame }))
  }

  close(): void {
    if (this.closed) return
    this.closed = true
    this.frames.length = 0
    for (const waiter of this.waiters.splice(0)) waiter.resolve(null)
    this.socket.close(1000, 'closed')
    const timer = setTimeout(() => this.socket.terminate(), 250)
    timer.unref()
  }
}

/** One bundled Core bridge process per LAN stream; it has no Node data directory or config. */
class CoreLanBridge implements FrameCarrier {
  private readonly child: ChildProcessWithoutNullStreams
  private readonly lines: ReturnType<typeof createInterface>
  private readonly frames: ClientFrame[] = []
  private readonly frameWaiters: { resolve(frame: ClientFrame | null): void; reject(error: Error): void }[] = []
  private readonly sendWaiters: { resolve(): void; reject(error: Error): void }[] = []
  private readonly opened: Promise<void>
  private resolveOpened!: () => void
  private rejectOpened!: (error: Error) => void
  private closed = false
  private failure: Error | null = null

  private constructor(program: string, deviceId: string, addrs: string[], timeoutMs: number) {
    if (!isAbsolute(program)) throw new Error('Client bridge path must be absolute')
    const env: NodeJS.ProcessEnv = { PATH: process.env.PATH ?? '' }
    if (process.env.SystemRoot) env.SystemRoot = process.env.SystemRoot
    this.child = spawn(program, ['--desktop-client-bridge'], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true, env })
    this.lines = createInterface({ input: this.child.stdout, crlfDelay: Infinity })
    this.opened = new Promise((resolveOpen, rejectOpen) => { this.resolveOpened = resolveOpen; this.rejectOpened = rejectOpen })
    this.lines.on('line', line => this.onLine(line))
    this.child.once('error', () => this.fail(new Error('LAN bridge could not start')))
    this.child.once('close', () => this.fail(new Error('LAN bridge closed')))
    this.child.stderr.resume()
    void this.write({ type: 'open', device_id: deviceId, addrs, timeout_ms: timeoutMs }).catch(() => this.fail(new Error('LAN bridge could not start')))
  }

  static async open(program: string, deviceId: string, addrs: string[], timeoutMs: number): Promise<CoreLanBridge> {
    const bridge = new CoreLanBridge(program, deviceId, addrs, timeoutMs)
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      await Promise.race([
        bridge.opened,
        new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error('LAN connection timed out')), timeoutMs + 5000) }),
      ])
      return bridge
    } catch (error) { bridge.close(); throw error }
    finally { if (timer) clearTimeout(timer) }
  }

  private onLine(line: string): void {
    if (line.length > 1_500_000) { this.fail(new Error('LAN bridge response too large')); return }
    let value: unknown
    try { value = JSON.parse(line) } catch { this.fail(new Error('Invalid LAN bridge response')); return }
    if (!isRecord(value) || typeof value.type !== 'string') { this.fail(new Error('Invalid LAN bridge response')); return }
    if (value.type === 'opened') { this.resolveOpened(); return }
    if (value.type === 'sent') { this.sendWaiters.shift()?.resolve(); return }
    if (value.type === 'closed') { this.finish(); return }
    if (value.type === 'error') { this.fail(new Error('LAN stream failed')); return }
    if (value.type === 'frame') {
      if (typeof value.text !== 'boolean' || typeof value.data !== 'string' || value.data.length > Math.ceil(MAX_LAN_FRAME * 4 / 3) + 8) { this.fail(new Error('Invalid LAN frame')); return }
      const data = Buffer.from(value.data, 'base64')
      if (data.length > MAX_LAN_FRAME) { this.fail(new Error('LAN frame too large')); return }
      const frame = { text: value.text, data: new Uint8Array(data) }
      const waiter = this.frameWaiters.shift()
      if (waiter) waiter.resolve(frame)
      else if (this.frames.length < MAX_QUEUED_FRAMES) this.frames.push(frame)
      else this.fail(new Error('LAN receive backpressure limit reached'))
      return
    }
    this.fail(new Error('Unknown LAN bridge response'))
  }

  private fail(error: Error): void {
    if (this.closed) return
    this.failure = error
    this.closed = true
    this.rejectOpened(error)
    for (const waiter of this.frameWaiters.splice(0)) waiter.reject(error)
    for (const waiter of this.sendWaiters.splice(0)) waiter.reject(error)
    this.lines.close()
    this.child.kill()
  }

  private finish(): void {
    if (this.closed) return
    this.closed = true
    this.frames.length = 0
    for (const waiter of this.frameWaiters.splice(0)) waiter.resolve(null)
    for (const waiter of this.sendWaiters.splice(0)) waiter.reject(new Error('LAN stream closed'))
    this.lines.close()
  }

  private write(value: Record<string, unknown>): Promise<void> {
    if (this.closed) return Promise.reject(this.failure ?? new Error('LAN stream closed'))
    const line = `${JSON.stringify(value)}\n`
    if (Buffer.byteLength(line) > Math.ceil(MAX_LAN_FRAME * 4 / 3) + 4096) return Promise.reject(new Error('LAN frame too large'))
    return new Promise((resolveWrite, rejectWrite) => {
      this.child.stdin.write(line, error => error ? rejectWrite(error) : resolveWrite())
    })
  }

  async send(bytes: Uint8Array): Promise<void> {
    if (bytes.byteLength > MAX_LAN_FRAME) throw new Error('LAN frame too large')
    const pending = new Promise<void>((resolveSend, rejectSend) => this.sendWaiters.push({ resolve: resolveSend, reject: rejectSend }))
    await this.write({ type: 'send', data: Buffer.from(bytes).toString('base64') })
    return pending
  }

  receive(): Promise<ClientFrame | null> {
    if (this.frames.length) return Promise.resolve(this.frames.shift() ?? null)
    if (this.failure) return Promise.reject(this.failure)
    if (this.closed) return Promise.resolve(null)
    if (this.frameWaiters.length) return Promise.reject(new Error('A LAN receive is already pending'))
    return new Promise((resolveFrame, rejectFrame) => this.frameWaiters.push({ resolve: resolveFrame, reject: rejectFrame }))
  }

  close(): void {
    if (this.closed) return
    void this.write({ type: 'close' }).catch(() => {})
    this.closed = true
    this.frames.length = 0
    for (const waiter of this.frameWaiters.splice(0)) waiter.resolve(null)
    for (const waiter of this.sendWaiters.splice(0)) waiter.reject(new Error('LAN stream closed'))
    this.child.stdin.end()
    this.lines.close()
    setTimeout(() => { if (this.child.exitCode === null) this.child.kill() }, 1500).unref()
  }
}

export function createClientHost(options: ClientHostOptions) {
  const cookies = new WeakMap<WebContents, Map<string, string>>()
  const sockets = new Map<string, LocalSocket>()
  const disposed = new Set<WebContents>()
  let root = ''
  let csp = ''
  let installed = false

  async function initAssets(): Promise<void> {
    if (root) return
    if (!isAbsolute(options.runtimeRoot)) throw new Error('Client asset root must be absolute')
    const candidate = resolve(options.runtimeRoot)
    const canonical = await realpath(candidate)
    const receiptPath = resolve(canonical, '.vibeface-build.json')
    const receipt = JSON.parse(await readFile(receiptPath, 'utf8')) as unknown
    csp = parseCsp(receipt)
    root = canonical
  }

  async function serve(request: Request): Promise<Response> {
    await initAssets()
    const url = new URL(request.url)
    if (url.protocol !== `${CLIENT_SCHEME}:` || url.host !== CLIENT_HOST || (request.method !== 'GET' && request.method !== 'HEAD')) return new Response(null, { status: 404 })
    const relativePath = pathForAsset(url.pathname)
    if (!relativePath) return new Response(null, { status: 403 })
    const target = resolve(root, relativePath)
    const relation = relative(root, target)
    if (!relation || relation === '..' || relation.startsWith(`..${sep}`) || isAbsolute(relation)) return new Response(null, { status: 403 })
    try {
      const canonicalTarget = await realpath(target)
      const resolvedRelation = relative(root, canonicalTarget)
      if (resolvedRelation === '..' || resolvedRelation.startsWith(`..${sep}`) || isAbsolute(resolvedRelation)) return new Response(null, { status: 403 })
      const info = await stat(canonicalTarget)
      if (!info.isFile()) return new Response(null, { status: 404 })
      const headers = {
        'content-type': MIME.get(extname(canonicalTarget).toLowerCase()) ?? 'application/octet-stream',
        'content-security-policy': csp,
        'cache-control': 'no-store',
        'x-content-type-options': 'nosniff',
        'content-length': String(info.size),
      }
      return new Response(request.method === 'HEAD' ? null : await readFile(canonicalTarget), { status: 200, headers })
    } catch { return new Response(null, { status: 404 }) }
  }

  async function currentTarget(): Promise<LocalTarget | null> {
    const current = await options.resolveSelectedNode()
    return selectedTarget(current)
  }

  async function discover(signal: AbortSignal): Promise<ClientLocalSetup | null> {
    const target = await currentTarget()
    if (!target || signal.aborted) return null
    const reply = await localHttp(target, { method: 'POST', path: '/api/local/setup', body: {}, signal })
    if (reply.status === 404) return null
    if (reply.status !== 200) throw new Error('Could not read local Node setup')
    return parseSetup(jsonBody(reply), target)
  }

  async function entry(contents: WebContents, deviceId: string, launch: ClientLocalLaunch | null): Promise<ClientLocalEntry | null> {
    if (!DEVICE_ID.test(deviceId)) throw new Error('Invalid device identity')
    const target = await currentTarget()
    if (!target) return null
    if (launch) {
      if (launch.deviceId !== target.nodeId || !HEX_TOKEN.test(launch.token) || typeof launch.rejoin !== 'boolean') throw new Error('Local launch belongs to another Node')
      const exchange = await localHttp(target, { method: 'POST', path: '/api/local/exchange', body: { token: launch.token, node_id: target.nodeId, device_id: deviceId } })
      if (exchange.status !== 200) throw new Error('Local launch expired or was already used')
      const response = jsonBody(exchange)
      const resultEntry = localEntry(response.entry, target, true)
      if (typeof response.network_id !== 'string' || !DEVICE_ID.test(response.network_id)) throw new Error('Invalid local session response')
      const values = exchange.headers['set-cookie']
      const expectedName = `vibeface_local_${target.nodeId}`
      const cookieHeader = Array.isArray(values) ? values : []
      const accepted = cookieHeader.map(value => value.split(';', 1)[0] ?? '').find(value => value.startsWith(`${expectedName}=`))
      const cookieValue = accepted?.slice(expectedName.length + 1)
      if (!cookieValue || !HEX_TOKEN.test(cookieValue) || !cookieHeader.some(value => value.includes('HttpOnly') && value.includes('SameSite=Strict') && value.includes('Path=/api/local'))) throw new Error('Invalid local session cookie')
      let byNode = cookies.get(contents)
      if (!byNode) { byNode = new Map(); cookies.set(contents, byNode) }
      byNode.set(target.nodeId, `${expectedName}=${cookieValue}`)
      return resultEntry
    }
    const cookie = cookies.get(contents)?.get(target.nodeId)
    const reply = await localHttp(target, { method: 'GET', path: '/api/local/info', ...(cookie ? { cookie } : {}) })
    if (reply.status !== 200) return null
    const value = jsonBody(reply)
    if (value.enabled !== true || value.has_session !== true || value.device_id !== deviceId) return null
    return localEntry(value.entry, target, true)
  }

  async function openCarrier(contents: WebContents, deviceId: string, timeoutMs: unknown): Promise<string> {
    if (!DEVICE_ID.test(deviceId) || typeof timeoutMs !== 'number' || !Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 30_000) throw new Error('Invalid Node identity')
    const target = await currentTarget()
    if (!target || target.nodeId !== deviceId) throw new Error('The selected local Node changed')
    const cookie = cookies.get(contents)?.get(target.nodeId)
    if (!cookie) throw new Error('A local session is required')
    const socket = await ClientWebSocket.open(target, '/api/local/ws', cookie, timeoutMs)
    const id = randomUUID()
    sockets.set(id, { nodeId: target.nodeId, contents, socket, receiving: false, sending: 0 })
    return id
  }

  async function openGateway(contents: WebContents, deviceId: string, address: unknown, timeoutMs: unknown): Promise<string | null> {
    if (!DEVICE_ID.test(deviceId) || typeof address !== 'string' || typeof timeoutMs !== 'number' || !Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 30_000) throw new Error('Invalid local Gateway target')
    const target = await currentTarget()
    if (!target) return null
    if (target.nodeId !== deviceId || target.origin !== address) return null
    const socket = await ClientWebSocket.open(target, '/api/client', null, timeoutMs, deviceId)
    const id = randomUUID()
    sockets.set(id, { nodeId: target.nodeId, contents, socket, receiving: false, sending: 0 })
    return id
  }

  async function openLan(contents: WebContents, deviceId: string, addrsValue: unknown, timeoutValue: unknown): Promise<string> {
    if (!DEVICE_ID.test(deviceId) || !Array.isArray(addrsValue) || addrsValue.length < 1 || addrsValue.length > MAX_LAN_ADDRESSES
        || typeof timeoutValue !== 'number' || !Number.isInteger(timeoutValue) || timeoutValue < 1 || timeoutValue > 30_000) throw new Error('Invalid LAN target')
    const addrs = [...new Set(addrsValue.map(value => {
      if (typeof value !== 'string' || value.length > 64) throw new Error('Invalid LAN address')
      const match = value.startsWith('[') ? /^\[([0-9a-fA-F:.]+)\]:(\d{1,5})$/.exec(value) : /^([0-9.]+):(\d{1,5})$/.exec(value)
      if (!match || !match[1] || !match[2] || isIP(match[1]) === 0) throw new Error('LAN address must use a numeric IP and port')
      const port = Number(match[2])
      if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid LAN port')
      return value
    }))]
    const bridge = await CoreLanBridge.open(options.clientBridgeProgram(), deviceId, addrs, timeoutValue)
    const id = randomUUID()
    sockets.set(id, { nodeId: deviceId, contents, socket: bridge, receiving: false, sending: 0 })
    return id
  }

  function socketFor(contents: WebContents, id: unknown): LocalSocket {
    if (typeof id !== 'string' || id.length > 64) throw new Error('Invalid local channel')
    const stream = sockets.get(id)
    if (!stream || stream.contents !== contents) throw new Error('Local channel is closed')
    return stream
  }

  function closeSocket(id: string): void {
    const stream = sockets.get(id)
    if (!stream) return
    sockets.delete(id)
    stream.socket.close()
  }

  function bind(contents: WebContents): void {
    if (disposed.has(contents)) return
    disposed.add(contents)
    contents.once('destroyed', () => {
      for (const [id, socket] of sockets) if (socket.contents === contents) closeSocket(id)
      cookies.delete(contents)
      disposed.delete(contents)
    })
  }

  function installIpc(): void {
    if (installed) throw new Error('Client IPC already installed')
    installed = true
    const handle = (channel: string, callback: (contents: WebContents, ...args: unknown[]) => unknown) => {
      ipcMain.handle(channel, (event, ...args) => {
        const contents = validateRenderer(event, options.getClientContents)
        bind(contents)
        return callback(contents, ...args)
      })
    }
    handle(CLIENT_IPC.localSetup, () => discover(new AbortController().signal))
    handle(CLIENT_IPC.localEntry, (contents, deviceId, launch) => {
      const validLaunch = launch === null ? null : isRecord(launch) && typeof launch.token === 'string' && typeof launch.deviceId === 'string' && typeof launch.rejoin === 'boolean'
        ? { token: launch.token, deviceId: launch.deviceId, rejoin: launch.rejoin } : null
      if (launch !== null && validLaunch === null) throw new Error('Invalid local launch')
      return entry(contents, String(deviceId), validLaunch)
    })
    handle(CLIENT_IPC.carrierOpen, (contents, deviceId, timeoutMs) => openCarrier(contents, String(deviceId), timeoutMs))
    handle(CLIENT_IPC.gatewayOpen, (contents, deviceId, address, timeoutMs) => openGateway(contents, String(deviceId), address, timeoutMs))
    handle(CLIENT_IPC.lanOpen, (contents, deviceId, addrs, timeoutMs) => openLan(contents, String(deviceId), addrs, timeoutMs))
    handle(CLIENT_IPC.carrierSend, async (contents, id, frame) => {
      const stream = socketFor(contents, id)
      const frameLimit = stream.socket instanceof ClientWebSocket ? MAX_WS_MESSAGE : MAX_LAN_FRAME
      if (!(frame instanceof Uint8Array) || frame.byteLength > frameLimit || stream.sending >= MAX_QUEUED_FRAMES) throw new Error('Invalid client frame or backpressure limit reached')
      stream.sending += 1
      try { await stream.socket.send(new Uint8Array(frame)) } finally { stream.sending -= 1 }
    })
    handle(CLIENT_IPC.carrierReceive, async (contents, id) => {
      const stream = socketFor(contents, id)
      if (stream.receiving) throw new Error('A local receive is already pending')
      stream.receiving = true
      try { return await stream.socket.receive() } finally { stream.receiving = false }
    })
    handle(CLIENT_IPC.carrierClose, (contents, id) => {
      const stream = socketFor(contents, id)
      closeSocket(typeof id === 'string' ? id : '')
      return undefined
    })
    handle(CLIENT_IPC.showNodePanel, () => options.showNodePanel())
  }

  function registerProtocol(): void {
    protocol.handle(CLIENT_SCHEME, serve)
  }

  function dispose(): void {
    for (const id of sockets.keys()) closeSocket(id)
    if (installed) {
      for (const channel of Object.values(CLIENT_IPC)) ipcMain.removeHandler(channel)
      installed = false
    }
  }

  return { clientUrl: CLIENT_URL, registerProtocol, installIpc, dispose }
}
