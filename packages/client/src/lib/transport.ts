import type { SecureApi, SecureChannel, SecureHandshake } from '../secure-api.ts'

export type DeviceChannel = {
  deviceId: string
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  close(): void
}
export type JoinLink = { origin: string | null; entryId: string; inviteId: string; inviteHash: string; secret: string }
/** This device's Ed25519 key as its platform keeps it: made once, it only ever signs. */
export type DeviceKeys = {
  /** The key, made on first use. Throws an error with `code: 'key_lost'` when a key this device made is gone, unless replacing it. */
  load(options: { replaceLostKey: boolean }): Promise<{ deviceId: string }>
  /** Drops the key; the next load makes a new one. */
  remove(): Promise<void>
  sign(message: Uint8Array): Promise<string>
}
type ConnectOptions = {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
  /** Connect and handshake must finish within this; entrance switching uses 5 seconds. */
  timeoutMs?: number
}
export type Transport = ReturnType<typeof createTransport>

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })
const devicePattern = /^[a-f0-9]{64}$/
const prologue = encoder.encode('vibeface:client:v1')
/** Frames from the secure module go to the socket as they are; DOM typings want an ArrayBuffer-backed view. */
const frame = (bytes: Uint8Array) => bytes as Uint8Array<ArrayBuffer>
/** The page this code runs in, if any; a phone app has none. */
const page = () => (globalThis as { location?: { origin: string; protocol: string } }).location

export function validTarget(value: unknown): string {
  if (typeof value !== 'string' || !devicePattern.test(value)) throw new Error('这个设备地址无效。请用加入链接或本机页面给的地址。')
  return value
}

export function validEndpoint(value: string): string {
  const url = new URL(value)
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash || !['', '/'].includes(url.pathname)) throw new Error('填写 HTTP 或 HTTPS 地址，不包含路径、账号或密钥。')
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)
  if (page()?.protocol === 'https:' && url.protocol === 'http:' && !local) throw new Error('HTTPS 页面只能连接 HTTPS 地址。')
  return url.origin
}

/** The ticket in a join QR code or link: `vibeface1…` alone or `https://<entry>/join#vibeface1…`. */
export function joinTicket(value: string): { ticket: string; origin: string | null } | null {
  const input = value.trim()
  if (input.startsWith('vibeface1')) return { ticket: input, origin: null }
  try {
    const url = new URL(input)
    if (url.pathname === '/join' && url.hash.startsWith('#vibeface1')) return { ticket: url.hash.slice(1), origin: url.origin }
  } catch {}
  return null
}

type Wire = {
  socket: WebSocket
  close(error?: Error): void
  ready(): void
  receive(): Promise<ArrayBuffer>
  opened(): Promise<void>
}

function openSocket(url: string, signal: AbortSignal | undefined, onClose: (error: Error) => void, timeoutMs: number): Wire {
  const socket = new WebSocket(url)
  socket.binaryType = 'arraybuffer'
  const incoming: ArrayBuffer[] = [], waiting: { resolve(value: ArrayBuffer): void; reject(error: Error): void }[] = []
  let failure: Error | undefined
  const timer = setTimeout(() => close(new Error('连接超时。')), timeoutMs)
  function close(error = new Error('无法接通指定设备。')) {
    if (failure) return
    failure = error
    clearTimeout(timer)
    signal?.removeEventListener('abort', abort)
    for (const item of waiting.splice(0)) item.reject(error)
    socket.close()
    onClose(error)
  }
  function abort() { close(new Error('设备连接已取消。')) }
  signal?.addEventListener('abort', abort, { once: true })
  socket.addEventListener('message', (event: MessageEvent) => {
    if (typeof event.data === 'string') {
      // The Gateway refuses in English for logs; `code` is what the page translates.
      const refusal = Object.assign(new Error('访问地址拒绝了这次连接。'), { code: undefined as unknown })
      try { const value = JSON.parse(event.data); refusal.message = String(value.error || refusal.message); refusal.code = value.code } catch {}
      close(refusal)
      return
    }
    const data = event.data as ArrayBuffer
    if (data.byteLength > 65535) { close(new Error('设备消息超过通道大小限制。')); return }
    const item = waiting.shift()
    if (item) item.resolve(data)
    else if (incoming.length < 512) incoming.push(data)
    else close(new Error('对方发送的数据过多，已关闭连接。'))
  })
  socket.addEventListener('close', () => close())
  socket.addEventListener('error', () => close(new Error('连不上这台电脑。请确认它开着，并且和这里网络互通。')))
  if (signal?.aborted) abort()
  return {
    socket, close, ready() { clearTimeout(timer) },
    receive() {
      const next = incoming.shift()
      if (next) return Promise.resolve(next)
      if (failure) return Promise.reject(failure)
      return new Promise((resolve, reject) => waiting.push({ resolve, reject }))
    },
    opened() {
      if (failure) return Promise.reject(failure)
      if (socket.readyState === WebSocket.OPEN) return Promise.resolve()
      return new Promise((resolve, reject) => {
        socket.addEventListener('open', () => resolve(), { once: true })
        socket.addEventListener('close', () => reject(failure || new Error('连接已断开。')), { once: true })
      })
    },
  }
}

export function createTransport(secure: SecureApi, keys: DeviceKeys) {
  let device: { deviceId: string } | undefined
  let initializing: Promise<{ deviceId: string }> | null = null

  function initClient({ replaceLostKey = false } = {}): Promise<{ deviceId: string }> {
    initializing ??= keys.load({ replaceLostKey }).then(loaded => {
      if (!devicePattern.test(loaded.deviceId)) throw new Error('这台设备保存的登录信息坏了。请重新加入。')
      device = { deviceId: loaded.deviceId }
      return { deviceId: loaded.deviceId }
    }).catch(error => { initializing = null; throw error })
    return initializing
  }

  /** A new device key for a device removed from its network; the old key stays revoked there and is dropped here. */
  async function replaceClient() {
    await initClient()
    await keys.remove()
    device = undefined; initializing = null
    return initClient({ replaceLostKey: true })
  }

  async function sign(bytes: Uint8Array) {
    if (!device) await initClient()
    return keys.sign(bytes)
  }

  /** Signs `vibeface:message:v1:{domain}:{sha256(payload)}`, the format every Node verifies. */
  async function signClient(domain: string, payload: string | Uint8Array) {
    await initClient()
    return sign(secure.signingMessage(domain, typeof payload === 'string' ? encoder.encode(payload) : payload))
  }

  /** Trust comes from the ticket's device key; the address in a link is only a hint. */
  async function parseJoinLink(value: string): Promise<JoinLink> {
    await initClient()
    if (value.trim().length > 8192) throw new Error('加入链接过长。请重新复制管理设备上的链接。')
    const found = joinTicket(value)
    if (!found) throw new Error('这不是 Vibeface 加入链接。')
    const ticket = JSON.parse(secure.ticketDecode(found.ticket))
    let origin: string | null = null
    try { origin = found.origin && validEndpoint(found.origin) } catch {}
    return { origin, entryId: ticket.entry_id, inviteId: ticket.invite_id, inviteHash: ticket.invite_hash, secret: ticket.secret }
  }

  /**
   * Opens a Noise channel to `entry.device_id` through the Gateway at `entry.address`.
   * The Node must prove that device key; any other key closes the connection.
   */
  async function connectDevice(entry: { address: string; device_id: string; local?: boolean }, { onEvent, onClose, signal, timeoutMs = 10000 }: ConnectOptions = {}): Promise<DeviceChannel> {
    if (signal?.aborted) throw new Error('设备连接已取消。')
    const me = await initClient()
    const target = validTarget(entry.device_id)
    const endpoint = validEndpoint(entry.address)
    if (entry.local === true && endpoint !== page()?.origin) throw new Error('本机会话只能连接这台电脑。')
    const url = new URL(entry.local === true ? '/api/local/ws' : '/api/client', endpoint)
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
    const pending = new Map<string, { resolve(value: unknown): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> }>()
    let channel: SecureChannel | undefined, closed = false, nextId = 0
    const disconnected = () => new Error('连接已断开，操作结果还没确认。重新连接后先确认原操作的结果，不要重复提交。')
    const wire = openSocket(url.href, signal, (error) => {
      closed = true
      const reason = channel ? disconnected() : error
      for (const item of pending.values()) { clearTimeout(item.timer); item.reject(reason) }
      pending.clear()
      onClose?.(reason)
    }, timeoutMs)
    const { socket, receive } = wire
    const fail = (error: Error) => wire.close(error)
    let handshake: SecureHandshake | undefined
    try {
      await wire.opened()
      if (entry.local !== true) socket.send(JSON.stringify({ type: 'connect', target }))
      handshake = new secure.Handshake(true, target, prologue)
      handshake.setProof(me.deviceId, await sign(handshake.signingBytes()))
      socket.send(frame(handshake.write(new Uint8Array())))
      const response = await receive()
      if (handshake.read(new Uint8Array(response)).length) throw new Error('连接设备时收到了意外数据，已断开。')
      socket.send(frame(handshake.write(new Uint8Array())))
      if (!handshake.isFinished() || handshake.remoteId() !== target) throw new Error('设备身份与目标编号不符，已拒绝连接。')
      channel = handshake.finish()
      wire.ready()
    } catch (error) {
      const reason = !(error as { code?: unknown })?.code && /device|identity|Proof|signature/i.test(String((error as Error)?.message ?? error)) ? new Error('设备身份与目标编号不符，已拒绝连接。') : error as Error
      fail(reason)
      throw reason
    } finally { handshake?.free() }
    const open = channel
    void (async () => {
      try {
        while (!closed) {
          const plaintext = open.openRecord(new Uint8Array(await receive()))
          if (plaintext == null) continue
          const message = JSON.parse(decoder.decode(plaintext))
          if (!message || typeof message !== 'object' || Array.isArray(message)) throw new Error('设备返回的加密消息格式无效。')
          if (message.id !== undefined && typeof message.id !== 'string') throw new Error('设备返回的请求编号格式无效。')
          if (message.error !== undefined && message.error !== null && typeof message.error !== 'string') throw new Error('设备返回的错误格式无效。')
          const item = message.id === undefined ? undefined : pending.get(message.id)
          if (item) {
            pending.delete(message.id); clearTimeout(item.timer)
            if (message.error !== undefined && message.error !== null) item.reject(Object.assign(new Error(message.error), { responded: true }))
            else item.resolve(message.result)
          } else onEvent?.(message)
        }
      } catch (error) { fail(error as Error) }
      finally { open.free() }
    })()
    return {
      deviceId: target,
      request(method, params = {}) {
        if (closed) return Promise.reject(disconnected())
        const id = String(++nextId)
        return new Promise((resolve, reject) => {
          const timer = setTimeout(() => { pending.delete(id); reject(new Error('设备还没确认结果。请先确认原操作的结果，不要重复提交。')) }, 60000)
          pending.set(id, { resolve, reject, timer })
          try { for (const record of open.sealMessage(encoder.encode(JSON.stringify({ id, method, params })))) socket.send(frame(record)) }
          catch (error) { fail(error as Error) }
        })
      },
      close() { fail(new Error('已断开设备连接。')) },
    }
  }

  return { initClient, replaceClient, signClient, parseJoinLink, connectDevice }
}
