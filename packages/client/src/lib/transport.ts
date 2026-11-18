import type { SecureApi, SecureChannel, SecureHandshake } from '../secure-api.ts'
import type { ConnectionEntry, PublicIdentity } from './node.ts'
import { parseHttpOrigin } from './origin.ts'

export type DeviceChannel = {
  deviceId: string
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  /** No request is waiting for its answer, so closing now leaves no outcome unknown. */
  idle(): boolean
  close(): void
}
/** The ticket's entry Node: its device ID, and the `ip:port`s it listens on when this device can dial it directly. */
export type JoinLink = { entryId: string; entryAddrs: string[]; inviteId: string; inviteHash: string; secret: string }
/** This device's Ed25519 key as its platform keeps it: made once, it only ever signs. */
export type DeviceKeys = {
  /** Load the key, creating it on first use unless reading back a replacement with `existingOnly`. */
  load(options: { replaceLostKey: boolean; existingOnly?: boolean }): Promise<{ deviceId: string }>
  /** Drops the key; the next load makes a new one. */
  remove(): Promise<void>
  sign(message: Uint8Array): Promise<string>
}
/** One client stream straight to a Node on this LAN; `recv` gives `null` once the Node ended it. */
export type LanStream = {
  send(data: Uint8Array): void
  recv(): Promise<{ text: boolean; data: Uint8Array } | null>
  close(): void
}
/** Opens a stream to Node `deviceId` at any of `addrs` (`ip:port`) within `timeoutMs`. */
export type LanDial = (deviceId: string, addrs: string[], timeoutMs: number) => Promise<LanStream>
/** Explicit native-only transport for the inspected local Node; browser local-origin checks stay in place. */
export type NativeLocalCarrier = (entry: Extract<ConnectionEntry, { kind: 'local' }>, timeoutMs: number) => Promise<LanStream>
/** Selected local Node's ordinary Gateway channel; remote Gateway entries remain browser WebSockets. */
export type NativeLocalGatewayCarrier = (entry: Extract<ConnectionEntry, { kind: 'gateway' }>, target: string, timeoutMs: number) => Promise<LanStream | null>
type ConnectOptions = {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
  /** Connect and handshake must finish within this; entrance switching uses less. */
  timeoutMs?: number
  /** A busy Gateway holds the connection in line: `position` 1 goes next, 0 is let through. */
  onQueued?: (position: number) => void
}
/** A computer that does not answer at its address: off, or on another network. */
export const unreachableText = '连不上这个节点。确认它正在运行，并且和这里网络互通。'

export type Transport = ReturnType<typeof createTransport>
/** What the Noise session runs over: binary frames each way; a text frame from the far side is a place in line or a refusal. */
type Carrier = {
  send(frame: Uint8Array): void
  receive(): Promise<Uint8Array>
  /** The handshake is done: the connect timeout stops. */
  ready(): void
  close(error?: Error): void
}

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })
const devicePattern = /^[a-f0-9]{64}$/
const ipPort = /^(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-fA-F:.]+\]):\d{1,5}$/
const prologue = encoder.encode('vibeface:client:v1')
/** Frames from the secure module go to the socket as they are; DOM typings want an ArrayBuffer-backed view. */
const frame = (bytes: Uint8Array) => bytes as Uint8Array<ArrayBuffer>
/** The page this code runs in, if any; a phone app has none. */
const page = () => (globalThis as { location?: { origin: string; protocol: string } }).location

// Keep aligned with Lane::Read in apps/core/src/command.rs. In particular,
// `conversation` changes the observation lease and must retain the Node's ordered lane.
const readMethods = new Set(['info', 'directory', 'write_state', 'node_settings', 'triage_settings', 'relay_settings', 'share_link', 'entrances'])
const readRpcMethods = new Set(['nodes', 'conversations', 'conversation_history', 'message_search', 'open_works', 'work_detail', 'workspace_overview', 'workspace_state', 'workspace_history', 'workspace_command_receipt', 'signed_receipt', 'input_receipt', 'scope_context', 'read_artifact', 'bots', 'questions', 'memory_state', 'notifications'])
const readCalls = 8, queuedReads = 64, requestTimeoutMs = 60000
const isRead = (method: string, params: Record<string, unknown>) => readMethods.has(method) || (method === 'rpc' && typeof params.method === 'string' && readRpcMethods.has(params.method))

export function validTarget(value: unknown): string {
  if (typeof value !== 'string' || !devicePattern.test(value)) throw new Error('这个设备地址无效。改用加入链接或本机页面给的地址。')
  return value
}

export function validEndpoint(value: string): string {
  const url = parseHttpOrigin(value)
  if (!url) throw new Error('填写 HTTP 或 HTTPS 地址，不包含路径、账号或密钥。')
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)
  if (page()?.protocol === 'https:' && url.protocol === 'http:' && !local) throw new Error('HTTPS 页面只能连接 HTTPS 地址。')
  return url.origin
}

/** Keep literal IP socket addresses; the URL parser supplies IP and port range checks. */
function isLanAddr(value: unknown): value is string {
  if (typeof value !== 'string' || !ipPort.test(value)) return false
  const split = value.lastIndexOf(':'), host = value.slice(0, split), port = Number(value.slice(split + 1))
  if (port < 1 || port > 65535) return false
  // IPv4 leading zeroes have octal meaning to URL, rather than the literal IP shown.
  if (!host.startsWith('[') && host.split('.').some(part => part.length > 1 && part.startsWith('0'))) return false
  try { return new URL(`http://${value}`).hostname !== '' } catch { return false }
}

/** The distinct `ip:port` in `value`, at most 16. */
export const lanAddrs = (value: unknown): string[] =>
  Array.isArray(value) ? [...new Set(value.filter(isLanAddr))].slice(0, 16) : []

const JOIN_LINK = 'vibeface://join/'
const ticketPattern = /^vibeface1[0-9a-z]+$/

/** The ticket in a join link or QR code: `vibeface://join/vibeface1…` or the ticket alone. */
export function joinTicket(value: string): string | null {
  const input = value.trim()
  const ticket = input.startsWith(JOIN_LINK) ? input.slice(JOIN_LINK.length) : input
  return ticketPattern.test(ticket) ? ticket : null
}

/** An invite as it is shared; the desktop and phone apps open it, a browser takes it pasted. */
export const joinLink = (ticket: string) => `${JOIN_LINK}${ticket}`

/** Frames not yet read, the connect timer and the one close of a carrier; `shut` drops what carries them. */
function inbox(signal: AbortSignal | undefined, timeoutMs: number, onClose: (error: Error) => void, shut: () => void, onQueued?: (position: number) => void) {
  const incoming: Uint8Array[] = [], waiting: { resolve(value: Uint8Array): void; reject(error: Error): void }[] = []
  let failure: Error | undefined, fail: (error: Error) => void = () => {}
  /** Rejects once the carrier closed, for racing what it waits on. */
  const failed = new Promise<never>((_, reject) => { fail = reject })
  failed.catch(() => {})
  const expire = () => close(new Error('连接超时。'))
  let timer = setTimeout(expire, timeoutMs)
  function close(error = new Error('无法接通指定设备。')) {
    if (failure) return
    failure = error
    clearTimeout(timer)
    signal?.removeEventListener('abort', abort)
    for (const item of waiting.splice(0)) item.reject(error)
    fail(error)
    shut()
    onClose(error)
  }
  function abort() { close(new Error('设备连接已取消。')) }
  signal?.addEventListener('abort', abort, { once: true })
  if (signal?.aborted) abort()
  return {
    failed, close,
    closed: () => failure !== undefined,
    ready() { clearTimeout(timer) },
    push(data: Uint8Array) {
      if (failure) return
      if (data.byteLength > 65535) { close(new Error('设备消息超过通道大小限制。')); return }
      const item = waiting.shift()
      if (item) item.resolve(data)
      else if (incoming.length < 512) incoming.push(data)
      else close(new Error('对方发送的数据过多，已关闭连接。'))
    },
    /**
     * A busy Gateway's place in line, sent while it waits and so its sign of life: the connect timeout starts over.
     * Anything else refuses, in English for logs; `code` is what the page translates.
     */
    text(text: string) {
      const refusal = Object.assign(new Error('访问地址拒绝了这次连接。'), { code: undefined as unknown })
      try {
        const value = JSON.parse(text)
        if (value?.type === 'queued' && Number.isSafeInteger(value.position)) {
          if (failure) return
          clearTimeout(timer); timer = setTimeout(expire, timeoutMs)
          onQueued?.(value.position)
          return
        }
        refusal.message = String(value.error || refusal.message); refusal.code = value.code
      } catch {}
      close(refusal)
    },
    receive(): Promise<Uint8Array> {
      const next = incoming.shift()
      if (next) return Promise.resolve(next)
      if (failure) return Promise.reject(failure)
      return new Promise((resolve, reject) => waiting.push({ resolve, reject }))
    },
  }
}

/** A WebSocket to a Gateway, which forwards to the Node `connect` names, or to the local page's own Node. */
async function socketCarrier(url: string, connect: string | null, signal: AbortSignal | undefined, onClose: (error: Error) => void, timeoutMs: number, onQueued?: (position: number) => void): Promise<Carrier> {
  const socket = new WebSocket(url)
  socket.binaryType = 'arraybuffer'
  const frames = inbox(signal, timeoutMs, onClose, () => socket.close(), onQueued)
  socket.addEventListener('message', (event: MessageEvent) => {
    if (typeof event.data === 'string') frames.text(event.data)
    else frames.push(new Uint8Array(event.data as ArrayBuffer))
  })
  socket.addEventListener('close', () => frames.close())
  socket.addEventListener('error', () => frames.close(new Error(unreachableText)))
  await Promise.race([new Promise(resolve => socket.addEventListener('open', resolve, { once: true })), frames.failed])
  if (connect) socket.send(connect)
  return { send: bytes => socket.send(frame(bytes)), receive: frames.receive, ready: frames.ready, close: frames.close }
}

/** A QUIC stream straight to the Node: no Gateway in between, so no connect frame. */
async function streamCarrier(openStream: () => Promise<LanStream>, signal: AbortSignal | undefined, onClose: (error: Error) => void, timeoutMs: number, onQueued?: (position: number) => void): Promise<Carrier> {
  let stream: LanStream | undefined
  const frames = inbox(signal, timeoutMs, onClose, () => stream?.close(), onQueued)
  const dialing = openStream().then(opened => {
    stream = opened
    if (frames.closed()) opened.close()
    return opened
  }, () => { throw new Error('在局域网里连不上这个节点。') })
  let opened: LanStream
  try { opened = await Promise.race([dialing, frames.failed]) }
  catch (error) { frames.close(error as Error); throw error }
  void (async () => {
    try {
      for (let next = await opened.recv(); next && !frames.closed(); next = await opened.recv()) {
        if (next.text) frames.text(new TextDecoder().decode(next.data))
        else frames.push(next.data)
      }
    } catch {}
    frames.close()
  })()
  // A closed stream drops what is sent, as a closed WebSocket does; the reason it closed is what `receive` gives.
  const send = (bytes: Uint8Array) => { if (!frames.closed()) try { opened.send(bytes) } catch { frames.close() } }
  return { send, receive: frames.receive, ready: frames.ready, close: frames.close }
}

async function lanCarrier(dial: LanDial, deviceId: string, addrs: string[], signal: AbortSignal | undefined, onClose: (error: Error) => void, timeoutMs: number): Promise<Carrier> {
  return streamCarrier(() => dial(deviceId, addrs, timeoutMs), signal, onClose, timeoutMs)
}

/** `dialLan`: the app's direct path to Nodes on its LAN; a browser has none. */
export function createTransport(secure: SecureApi, keys: DeviceKeys, dialLan?: LanDial, nativeLocalCarrier?: NativeLocalCarrier, nativeLocalGateway?: NativeLocalGatewayCarrier) {
  let device: PublicIdentity | undefined
  let initializing: Promise<PublicIdentity> | null = null
  let changingKeys: Promise<unknown> | null = null
  let pendingReplacement: { previousId: string; publish?: (identity: PublicIdentity) => void } | null = null
  const cancelled = () => new Error('设备连接已取消。')

  function loadClient({ replaceLostKey = false } = {}): Promise<PublicIdentity> {
    initializing ??= keys.load({ replaceLostKey, existingOnly: pendingReplacement !== null && !replaceLostKey }).then(loaded => {
      if (!devicePattern.test(loaded.deviceId)) throw new Error('这台设备保存的登录信息坏了。需要重新加入。')
      const identity = { deviceId: loaded.deviceId }
      if (pendingReplacement) {
        if (identity.deviceId === pendingReplacement.previousId) throw new Error('还无法确认新的设备登录信息。重新连接后再试。')
        pendingReplacement.publish?.(identity)
        pendingReplacement = null
      }
      device = identity
      return device
    }).catch(error => { initializing = null; throw error })
    return initializing
  }

  function initClient(options?: { replaceLostKey?: boolean }): Promise<PublicIdentity> {
    if (changingKeys) {
      const load = () => initClient(options)
      return changingKeys.then(load, load)
    }
    return loadClient(options)
  }
  function blockKeyUse<Result>(operation: Promise<Result>): Promise<Result> {
    const work = operation.finally(() => { if (changingKeys === work) changingKeys = null })
    changingKeys = work
    return work
  }
  async function dropKey() {
    await keys.remove()
    device = undefined; initializing = null; pendingReplacement = null
  }

  /** Publish the persisted replacement before a new connection can use it. */
  function replaceClient({ beforeRemove, onReplaced }: {
    beforeRemove?: () => void; onReplaced?: (identity: PublicIdentity) => void
  } = {}): Promise<PublicIdentity> {
    if (changingKeys) return Promise.reject(cancelled())
    if (pendingReplacement) return loadClient({ replaceLostKey: true })
    return blockKeyUse(loadClient().then(async previous => {
      beforeRemove?.()
      // A failed removal leaves the initialized key available. Once removal succeeds, reload only the new key.
      await dropKey()
      // Persistence can finish before load reports an error. Keep the publisher for the first successful readback.
      pendingReplacement = { previousId: previous.deviceId, publish: onReplaced }
      return loadClient({ replaceLostKey: true })
    }))
  }

  /** Deletes this device's key here and drops it from memory; the next initClient makes a new one. */
  function forgetClient() {
    return blockKeyUse(Promise.resolve(changingKeys ?? initializing).then(dropKey, dropKey))
  }

  function requireSigningIdentity(identity: PublicIdentity) {
    if (changingKeys || device !== identity) throw cancelled()
  }
  async function sign(identity: PublicIdentity, bytes: Uint8Array) {
    requireSigningIdentity(identity)
    const signature = await keys.sign(bytes)
    requireSigningIdentity(identity)
    return signature
  }

  /** Signs `vibeface:message:v1:{domain}:{sha256(payload)}`, the format every Node verifies. */
  async function signClient(domain: string, payload: string | Uint8Array) {
    if (changingKeys || pendingReplacement) throw cancelled()
    const identity = await initClient()
    return sign(identity, secure.signingMessage(domain, typeof payload === 'string' ? encoder.encode(payload) : payload))
  }

  /** Trust comes from the ticket's device key. */
  async function parseJoinLink(value: string): Promise<JoinLink> {
    await initClient()
    if (value.trim().length > 8192) throw new Error('加入链接过长。重新复制管理设备上的链接。')
    const found = joinTicket(value)
    if (!found) throw new Error('这不是 Vibeface 加入链接。')
    const ticket = JSON.parse(secure.ticketDecode(found))
    // The node link's relay is not passed on: the app's direct dial takes addresses only.
    const entryAddrs = dialLan ? lanAddrs(new URL(ticket.entry).searchParams.getAll('addr')) : []
    return { entryId: ticket.entry_id, entryAddrs, inviteId: ticket.invite_id, inviteHash: ticket.invite_hash, secret: ticket.secret }
  }

  async function carry(entry: ConnectionEntry, target: string, signal: AbortSignal | undefined, onClose: (error: Error) => void, timeoutMs: number, onQueued?: (position: number) => void): Promise<Carrier> {
    if (entry.kind === 'lan') {
      if (!dialLan) throw new Error('这里不能在局域网里直连设备。')
      const addrs = lanAddrs(entry.addrs)
      if (!addrs.length) throw new Error('访问地址缺失或格式无效。')
      return lanCarrier(dialLan, target, addrs, signal, onClose, timeoutMs)
    }
    const endpoint = validEndpoint(entry.address)
    if (entry.kind === 'local' && nativeLocalCarrier) {
      return streamCarrier(() => nativeLocalCarrier(entry, timeoutMs), signal, onClose, timeoutMs)
    }
    if (entry.kind === 'local' && endpoint !== page()?.origin) throw new Error('本机会话只能连接这个节点。')
    if (entry.kind === 'gateway' && nativeLocalGateway) {
      const stream = await nativeLocalGateway(entry, target, timeoutMs)
      if (stream) return streamCarrier(() => Promise.resolve(stream), signal, onClose, timeoutMs, onQueued)
    }
    const url = new URL(entry.kind === 'local' ? '/api/local/ws' : '/api/client', endpoint)
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
    return socketCarrier(url.href, entry.kind === 'local' ? null : JSON.stringify({ type: 'connect', target }), signal, onClose, timeoutMs, onQueued)
  }

  /**
   * Opens a Noise channel to `entry.device_id` over the way `entry` names.
   * The Node must prove that device key; any other key closes the connection.
   */
  async function connectDevice(entry: ConnectionEntry, { onEvent, onClose, signal, timeoutMs = 10000, onQueued }: ConnectOptions = {}): Promise<DeviceChannel> {
    if (signal?.aborted) throw cancelled()
    const me = await initClient()
    if (signal?.aborted) throw cancelled()
    requireSigningIdentity(me)
    const target = validTarget(entry.device_id)
    type Reply = { resolve(value: unknown): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> }
    const pending = new Map<string, Reply & { read: boolean }>()
    const waitingReads: (Reply & { id: string; plaintext: Uint8Array })[] = []
    let activeReads = 0
    let channel: SecureChannel | undefined, closed = false, nextId = 0
    const disconnected = () => new Error('连接已断开，操作结果还没确认。重新连接后先确认原操作的结果，不要重复提交。')
    const closing = (error: Error) => {
      closed = true
      const reason = channel ? disconnected() : error
      for (const item of pending.values()) { clearTimeout(item.timer); item.reject(reason) }
      pending.clear()
      for (const item of waitingReads.splice(0)) {
        clearTimeout(item.timer)
        item.reject(new Error('连接已断开，排队的读取尚未发出。重新连接后再读取。'))
      }
      activeReads = 0
      onClose?.(reason)
    }
    let carrier: Carrier | undefined, handshake: SecureHandshake | undefined
    try {
      carrier = await carry(entry, target, signal, closing, timeoutMs, onQueued)
      if (signal?.aborted) throw cancelled()
      handshake = new secure.Handshake(true, target, prologue)
      const proof = await sign(me, handshake.signingBytes())
      requireSigningIdentity(me)
      handshake.setProof(me.deviceId, proof)
      carrier.send(handshake.write(new Uint8Array()))
      if (handshake.read(await carrier.receive()).length) throw new Error('连接设备时收到了意外数据，已断开。')
      requireSigningIdentity(me)
      carrier.send(handshake.write(new Uint8Array()))
      if (!handshake.isFinished() || handshake.remoteId() !== target) throw new Error('设备身份与目标编号不符，已拒绝连接。')
      channel = handshake.finish()
      carrier.ready()
    } catch (error) {
      const reason = !(error as { code?: unknown })?.code && /device|identity|Proof|signature/i.test(String((error as Error)?.message ?? error)) ? new Error('设备身份与目标编号不符，已拒绝连接。') : error as Error
      carrier?.close(reason)
      throw reason
    } finally { handshake?.free() }
    const open = channel, wire = carrier
    function send(id: string, plaintext: Uint8Array, read: boolean, resolve: Reply['resolve'], reject: Reply['reject']) {
      if (read) activeReads++
      const timer = setTimeout(() => {
        pending.delete(id)
        const error = new Error('设备还没确认结果。先确认原操作的结果，别重复提交。')
        reject(error)
        // A local timeout does not cancel work on the Node. Close a stalled read
        // connection instead of releasing its slot and exceeding the remote budget.
        if (read) wire.close(error)
      }, requestTimeoutMs)
      pending.set(id, { resolve, reject, timer, read })
      try { for (const record of open.sealMessage(plaintext)) wire.send(record) }
      catch (error) { wire.close(error as Error) }
    }
    function drainReads() {
      while (!closed && activeReads < readCalls && waitingReads.length) {
        const item = waitingReads.shift()!
        clearTimeout(item.timer)
        send(item.id, item.plaintext, true, item.resolve, item.reject)
      }
    }
    void (async () => {
      try {
        while (!closed) {
          const plaintext = open.openRecord(await wire.receive())
          if (plaintext == null) continue
          const message = JSON.parse(decoder.decode(plaintext))
          if (!message || typeof message !== 'object' || Array.isArray(message)) throw new Error('设备返回的加密消息格式无效。')
          if (message.id !== undefined && typeof message.id !== 'string') throw new Error('设备返回的请求编号格式无效。')
          if (message.error !== undefined && message.error !== null && typeof message.error !== 'string') throw new Error('设备返回的错误格式无效。')
          const item = message.id === undefined ? undefined : pending.get(message.id)
          if (item) {
            pending.delete(message.id); clearTimeout(item.timer)
            if (item.read) activeReads--
            if (message.error !== undefined && message.error !== null) item.reject(Object.assign(new Error(message.error), { responded: true, code: typeof message.code === 'string' ? message.code : undefined }))
            else item.resolve(message.result)
            drainReads()
          } else if (message.id === undefined) onEvent?.(message)
        }
      } catch (error) { wire.close(error as Error) }
      finally { open.free() }
    })()
    return {
      deviceId: target,
      request(method, params = {}) {
        if (closed) return Promise.reject(disconnected())
        const read = isRead(method, params)
        if (read && activeReads >= readCalls && waitingReads.length >= queuedReads) return Promise.reject(Object.assign(new Error('等待读取的请求过多。稍后再读取。'), { code: 'client_overloaded' }))
        const id = String(++nextId)
        return new Promise((resolve, reject) => {
          // Snapshot params now; seal only when sent to preserve Noise frame order.
          const plaintext = encoder.encode(JSON.stringify({ id, method, params }))
          if (!read || activeReads < readCalls) { send(id, plaintext, read, resolve, reject); return }
          const timer = setTimeout(() => {
            const index = waitingReads.findIndex(item => item.id === id)
            if (index !== -1) waitingReads.splice(index, 1)
            reject(new Error('等待读取超时，这次读取尚未发出。稍后再读取。'))
          }, requestTimeoutMs)
          waitingReads.push({ id, plaintext, resolve, reject, timer })
        })
      },
      idle: () => pending.size === 0 && waitingReads.length === 0,
      close() { wire.close(new Error('已断开设备连接。')) },
    }
  }

  return { initClient, replaceClient, forgetClient, signClient, parseJoinLink, connectDevice }
}
