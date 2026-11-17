import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { createTransport, joinTicket, type LanDial, type LanStream } from '@vibeface/client/lib/transport.ts'
import type { ConnectionEntry } from '@vibeface/client/lib/node.ts'
import type { SecureApi } from '@vibeface/client/secure-api.ts'
import initCrypto, * as wasm from '../../generated/vibeface_secure.js'
import { Handshake } from '../../generated/vibeface_secure.js'

const hex = (bytes: ArrayBuffer) => [...new Uint8Array(bytes)].map(value => value.toString(16).padStart(2, '0')).join('')
const prologue = new TextEncoder().encode('vibeface:client:v1')
const settle = () => new Promise(setImmediate)

async function key() {
  const pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']) as CryptoKeyPair
  return { privateKey: pair.privateKey, deviceId: hex(await crypto.subtle.exportKey('raw', pair.publicKey)) }
}

/**
 * A fake Gateway, or with `lan` a Node on the LAN, that answers as `serving` (normally the target) or refuses with a text frame.
 * With `queue` it first holds the session in line, telling each place every `noticeMs`; a place below 0 is a full line.
 */
async function harness({ local = false, lan = false, refuse = '', impostor = false, nativeLocal = false, nativeGateway = false, queue = [] as number[], noticeMs = 0, holdRequests = false } = {}) {
  await initCrypto({ module_or_path: readFileSync(new URL('../../generated/vibeface_secure_bg.wasm', import.meta.url)) })
  const browser = await key(), target = await key(), serving = impostor ? await key() : target
  const urls: string[] = [], texts: string[] = []
  const requests: { id: string; method: string; params: Record<string, unknown> }[] = []
  const replies = new Map<string, (result: unknown, error?: string, code?: string) => void>()
  let open = 0
  class GatewaySocket extends EventTarget {
    static OPEN = 1
    readyState = 0
    binaryType = 'arraybuffer'
    handshake: Handshake | undefined
    channel: ReturnType<Handshake['finish']> | undefined
    dead = false
    held: (string | Uint8Array)[] | undefined
    readonly url: string
    constructor(url: string) {
      super(); this.url = url; urls.push(url); open++
      queueMicrotask(async () => { this.readyState = 1; await this.respond(); this.dispatchEvent(new Event('open')) })
    }
    async respond() {
      const handshake = new Handshake(false, undefined, prologue)
      handshake.setProof(serving.deviceId, hex(await crypto.subtle.sign({ name: 'Ed25519' }, serving.privateKey, new Uint8Array(handshake.signingBytes()))))
      this.handshake = handshake
    }
    emit(data: unknown) { if (!this.dead) queueMicrotask(() => this.dispatchEvent(new MessageEvent('message', { data }))) }
    async wait() {
      for (const position of queue) {
        await new Promise(resolve => setTimeout(resolve, noticeMs))
        this.emit(JSON.stringify(position < 0 ? { type: 'error', code: 'gateway_busy', error: 'this Gateway relays too many sessions' } : { type: 'queued', position }))
      }
      const held = this.held ?? []
      this.held = undefined
      for (const value of held) this.send(value)
    }
    send(value: string | Uint8Array) {
      if (this.dead) return
      if (this.held) { this.held.push(value); return }
      if (typeof value === 'string') {
        texts.push(value)
        if (refuse) { this.emit(JSON.stringify({ type: 'error', error: refuse })); this.handshake?.free(); this.handshake = undefined }
        else if (queue.length) { this.held = []; void this.wait() }
        return
      }
      if (this.handshake) {
        this.handshake.read(value)
        if (this.handshake.isFinished()) { this.channel = this.handshake.finish(); this.handshake.free(); this.handshake = undefined }
        else this.emit(new Uint8Array(this.handshake.write(new Uint8Array())).buffer)
      } else if (this.channel) {
        const request = this.channel.openRecord(value)
        if (!request) return
        const message = JSON.parse(new TextDecoder().decode(request))
        requests.push(message)
        const reply = (result: unknown, error?: string, code?: string) => {
          replies.delete(message.id)
          for (const frame of this.channel!.sealMessage(new TextEncoder().encode(JSON.stringify({ id: message.id, result, error, code })))) this.emit(new Uint8Array(frame).buffer)
        }
        if (holdRequests) replies.set(message.id, reply)
        else reply({ device_id: serving.deviceId })
      }
    }
    close() {
      if (this.dead) return
      this.dead = true; open--
      this.handshake?.free(); this.channel?.free(); this.handshake = undefined; this.channel = undefined
      this.dispatchEvent(new Event('close'))
    }
  }
  Object.assign(globalThis, { WebSocket: GatewaySocket, location: { protocol: 'https:', origin: 'https://gateway.example' } })
  const dials: { deviceId: string; addrs: string[] }[] = []
  /** The same Node over a QUIC stream: frames as the native stream gives them, a refusal as text. */
  const dial: LanDial = async (deviceId, addrs) => {
    dials.push({ deviceId, addrs })
    if (addrs[0] === '192.168.1.99:7001') throw new Error('timed out')
    const node = new GatewaySocket(`quic://${addrs[0]}`)
    const queue: Awaited<ReturnType<LanStream['recv']>>[] = [], readers: ((frame: Awaited<ReturnType<LanStream['recv']>>) => void)[] = []
    const put = (frame: Awaited<ReturnType<LanStream['recv']>>) => { const reader = readers.shift(); if (reader) reader(frame); else queue.push(frame) }
    node.addEventListener('message', (event) => {
      const { data } = event as MessageEvent
      put(typeof data === 'string' ? { text: true, data: new TextEncoder().encode(data) } : { text: false, data: new Uint8Array(data) })
    })
    node.addEventListener('close', () => put(null))
    await new Promise(resolve => node.addEventListener('open', resolve, { once: true }))
    if (refuse) node.emit(JSON.stringify({ error: refuse, code: 'target_not_member' }))
    return {
      send: (data) => node.send(data),
      recv: () => queue.length ? Promise.resolve(queue.shift()!) : new Promise(resolve => readers.push(resolve)),
      close: () => node.close(),
    }
  }
  const secure: SecureApi = wasm
  const nativeCalls: { kind: string; deviceId: string; address?: string }[] = []
  const nativeStream: LanStream = { send: () => {}, recv: async () => null, close: () => {} }
  const runtime = createTransport(secure, {
    load: async () => ({ deviceId: browser.deviceId }), remove: async () => {},
    sign: async (message) => hex(await crypto.subtle.sign({ name: 'Ed25519' }, browser.privateKey, new Uint8Array(message))),
  }, lan ? dial : undefined,
  nativeLocal ? async entry => { nativeCalls.push({ kind: entry.kind, deviceId: entry.device_id }); return nativeStream } : undefined,
  nativeGateway ? async (entry, target) => { nativeCalls.push({ kind: entry.kind, deviceId: target, address: entry.address }); return null } : undefined)
  const address = 'https://gateway.example', device_id = target.deviceId
  const entry: ConnectionEntry = lan ? { kind: 'lan', addrs: ['192.168.1.4:7001'], device_id } : { kind: local ? 'local' : 'gateway', address, device_id }
  return {
    target: target.deviceId, urls, texts, dials, nativeCalls, requests, open: () => open,
    reply: (id: string, result: unknown, error?: string, code?: string) => {
      const reply = replies.get(id)
      assert.ok(reply, `request ${id} reached the Node`)
      reply(result, error, code)
    },
    connect: (to = entry, options: Parameters<typeof runtime.connectDevice>[1] = {}) => runtime.connectDevice(to, options),
    parseJoinLink: (value: string) => runtime.parseJoinLink(value),
  }
}

test('the gateway receives a connect frame and the Noise session proves the target device', async () => {
  const app = await harness()
  const channel = await app.connect()
  assert.deepEqual(app.urls, ['wss://gateway.example/api/client'])
  assert.deepEqual(app.texts.map(text => JSON.parse(text)), [{ type: 'connect', target: app.target }])
  assert.equal(channel.deviceId, app.target)
  assert.equal((await channel.request('info') as { device_id: string }).device_id, app.target)
  channel.close(); await settle()
  assert.equal(app.open(), 0)
})

test('a Node that proves another device id is refused and its socket closed', async () => {
  const app = await harness({ impostor: true })
  await assert.rejects(app.connect(), /设备身份与目标编号不符/)
  await settle()
  assert.equal(app.open(), 0)
})

test('a text frame from the gateway ends the attempt with its reason', async () => {
  const app = await harness({ refuse: 'this Gateway can only reach itself' })
  await assert.rejects(app.connect(), /this Gateway can only reach itself/)
  await settle()
  assert.equal(app.open(), 0)
})

test('a busy gateway holds the session in line, each place it tells restarting the connect timeout', async () => {
  const app = await harness({ queue: [3, 2, 1, 0], noticeMs: 50 })
  const places: number[] = []
  const channel = await app.connect(undefined, { timeoutMs: 150, onQueued: position => places.push(position) })
  assert.deepEqual(places, [3, 2, 1, 0])
  assert.equal((await channel.request('info') as { device_id: string }).device_id, app.target)
  channel.close(); await settle()
  assert.equal(app.open(), 0)
})

test('a gateway whose line is full refuses with gateway_busy', async () => {
  const app = await harness({ queue: [1, -1] })
  await assert.rejects(app.connect(), (error: Error & { code?: unknown }) => error.code === 'gateway_busy' && /too many sessions/.test(error.message))
  await settle()
  assert.equal(app.open(), 0)
})

test('a local session uses the cookie endpoint and sends no connect frame', async () => {
  const app = await harness({ local: true })
  const channel = await app.connect()
  assert.deepEqual(app.urls, ['wss://gateway.example/api/local/ws'])
  assert.deepEqual(app.texts, [])
  channel.close()
})

test('an explicit Desktop local carrier handles a local entry without weakening the browser origin guard', async () => {
  const native = await harness({ local: true, nativeLocal: true })
  await assert.rejects(native.connect(), /无法接通指定设备/)
  assert.deepEqual(native.nativeCalls, [{ kind: 'local', deviceId: native.target }])
  assert.deepEqual(native.urls, [])
  const browser = await harness({ local: true })
  await assert.rejects(browser.connect({ kind: 'local', address: 'https://other.example', device_id: browser.target }), /本机会话只能连接这个节点/)
})

test('the explicit Desktop gateway hook leaves remote gateways on the existing browser carrier', async () => {
  const app = await harness({ nativeGateway: true })
  const channel = await app.connect()
  assert.deepEqual(app.nativeCalls, [{ kind: 'gateway', deviceId: app.target, address: 'https://gateway.example' }])
  assert.deepEqual(app.urls, ['wss://gateway.example/api/client'])
  assert.equal(channel.deviceId, app.target)
  channel.close(); await settle()
})

test('the app reaches a Node on its LAN directly: no Gateway, no connect frame, and Noise still proves the device', async () => {
  const app = await harness({ lan: true })
  const channel = await app.connect()
  assert.deepEqual(app.dials, [{ deviceId: app.target, addrs: ['192.168.1.4:7001'] }])
  assert.deepEqual([app.urls, app.texts], [['quic://192.168.1.4:7001'], []])
  assert.equal((await channel.request('info') as { device_id: string }).device_id, app.target)
  channel.close(); await settle()
  assert.equal(app.open(), 0)
  await assert.rejects(app.connect({ kind: 'lan', addrs: ['192.168.1.99:7001'], device_id: app.target }), /在局域网里连不上这个节点/)
})

test('on the LAN, an impostor is refused and a text frame is the Node refusing', async () => {
  const impostor = await harness({ lan: true, impostor: true })
  await assert.rejects(impostor.connect(), /设备身份与目标编号不符/)
  await settle()
  assert.equal(impostor.open(), 0)
  const refused = await harness({ lan: true, refuse: 'unknown member' })
  await assert.rejects(refused.connect(), (error: Error & { code?: string }) => error.message === 'unknown member' && error.code === 'target_not_member')
  await settle()
  assert.equal(refused.open(), 0)
})

test('a browser has no LAN path', async () => {
  const app = await harness()
  await assert.rejects(app.connect({ kind: 'lan', addrs: ['192.168.1.4:7001'], device_id: app.target }), /这里不能在局域网里直连设备/)
})

test('a join link and a bare ticket name the same ticket, and no other shape is one', () => {
  assert.equal(joinTicket('vibeface://join/vibeface1test'), 'vibeface1test')
  assert.equal(joinTicket(' vibeface1test '), 'vibeface1test')
  for (const text of ['https://node.example/join#vibeface1test', 'vibeface://join?link=vibeface1test', 'vibeface://join/vibeface1test?x=1', 'vibeface://other/vibeface1test', 'https://node.example/#vibeface1test', '']) {
    assert.equal(joinTicket(text), null, text)
  }
})

test('a join link carries the entry Node\'s node link', async () => {
  const app = await harness()
  const entry = `vibeface://node/${app.target}?addr=203.0.113.5:4790&relay=https://relay.example`
  assert.equal(JSON.parse(wasm.ticketDecode(wasm.ticketEncode(entry, '1'.repeat(64)))).entry, entry)
  assert.throws(() => wasm.ticketEncode(app.target, '1'.repeat(64)), 'a bare device ID is no node link')
  const ticket = wasm.ticketEncode(entry, '1'.repeat(64))
  const link = await app.parseJoinLink(`vibeface://join/${ticket}`)
  assert.deepEqual(Object.keys(link).sort(), ['entryAddrs', 'entryId', 'inviteHash', 'inviteId', 'secret'])
  assert.equal(link.entryId, app.target)
  assert.deepEqual(link.entryAddrs, [], 'a browser cannot dial the entry directly')
  assert.deepEqual((await (await harness({ lan: true })).parseJoinLink(ticket)).entryAddrs, ['203.0.113.5:4790'])
  assert.equal(link.secret, '1'.repeat(64))
  assert.deepEqual(await app.parseJoinLink(ticket), link)
  await assert.rejects(app.parseJoinLink(`https://app.vibeface.dev/join#${ticket}`), /不是 Vibeface 加入链接/)
})

test('the device key cannot be exported', async () => {
  const { privateKey } = await key()
  assert.equal(privateKey.extractable, false)
  await assert.rejects(crypto.subtle.exportKey('pkcs8', privateKey))
  await assert.rejects(crypto.subtle.exportKey('jwk', privateKey))
})

test('workspace reads share the Node read budget and retain correlation when responses arrive out of order', async () => {
  const app = await harness({ holdRequests: true })
  const channel = await app.connect()
  const calls = [
    ['directory', {}], ['write_state', {}],
    ...['nodes', 'notifications', 'bots', 'questions', 'open_works', 'workspace_overview', 'workspace_state', 'work_detail', 'conversation_history', 'memory_state', 'message_search'].map(method => ['rpc', { method }] as const),
  ] as const
  const results = calls.map(([method, params]) => channel.request(method, params))
  const completed = Promise.allSettled(results)
  try {
    assert.equal(app.requests.length, 8, 'a cold refresh must not exceed Core READ_CALLS')
    assert.equal(channel.idle(), false)
    app.reply('8', { request: 8 })
    await settle()
    assert.equal(app.requests.length, 9, 'only a completed read admits the next one')
    app.reply('3', null, 'not available', 'fixture_refused')
    await settle()
    assert.equal(app.requests.length, 10, 'a refused read also releases its slot')
    for (const id of ['1', '2', '4', '5', '6', '7']) app.reply(id, { request: Number(id) })
    await settle()
    for (const id of ['13', '12', '10', '9', '11']) app.reply(id, { request: Number(id) })
    const settled = await completed
    settled.forEach((result, index) => {
      if (index === 2) {
        assert.equal(result.status, 'rejected')
        if (result.status === 'rejected') assert.equal(result.reason.code, 'fixture_refused')
      } else assert.deepEqual(result, { status: 'fulfilled', value: { request: index + 1 } })
    })
    assert.equal(channel.idle(), true)
  } finally { channel.close(); await completed }
})

test('read backpressure leaves writes, observation, long calls and stop controls available without replay', async () => {
  const app = await harness({ holdRequests: true })
  const channel = await app.connect()
  const calls = Array.from({ length: 9 }, () => channel.request('write_state'))
  calls.push(channel.request('rpc', { method: 'send_message' }), channel.request('rpc', { method: 'conversation' }), channel.request('provider_models'), channel.request('rpc', { method: 'interrupt_work' }), channel.request('rpc', { method: 'cancel_manager' }))
  const completed = Promise.allSettled(calls)
  try {
    assert.deepEqual(app.requests.slice(8).map(item => [item.method, item.params.method]), [
      ['rpc', 'send_message'], ['rpc', 'conversation'], ['provider_models', undefined], ['rpc', 'interrupt_work'], ['rpc', 'cancel_manager'],
    ])
    assert.deepEqual(app.requests.map(item => item.id), ['1', '2', '3', '4', '5', '6', '7', '8', '10', '11', '12', '13', '14'])
    channel.close()
    const settled = await completed
    assert.equal(settled.every(item => item.status === 'rejected'), true)
    const write = settled[9]
    assert.equal(write.status, 'rejected')
    if (write.status === 'rejected') {
      assert.match(write.reason.message, /操作结果还没确认/)
      assert.equal(write.reason.responded, undefined)
    }
    assert.equal(app.requests.length, 13, 'no queued read or write is replayed on close')
    assert.equal(channel.idle(), true)
  } finally { channel.close(); await completed }
})

test('read waiting is bounded and closing rejects both sent and queued reads', async () => {
  const app = await harness({ holdRequests: true })
  const channel = await app.connect()
  const calls = Array.from({ length: 72 }, () => channel.request('write_state'))
  const completed = Promise.allSettled(calls)
  try {
    await assert.rejects(channel.request('directory'), (error: Error & { code?: string }) => error.code === 'client_overloaded')
    assert.equal(app.requests.length, 8)
    channel.close()
    const settled = await completed
    assert.equal(settled.length, 72)
    assert.equal(settled.every(item => item.status === 'rejected'), true)
    for (const result of settled.slice(8)) {
      if (result.status === 'rejected') assert.match(result.reason.message, /尚未发出/)
    }
    assert.equal(channel.idle(), true)
    assert.equal(app.open(), 0)
  } finally { channel.close(); await completed }
})

test('a queued read receives its full response deadline when sent and a stalled read closes without releasing a live remote slot', async t => {
  const app = await harness({ holdRequests: true })
  const events: unknown[] = []
  const channel = await app.connect(undefined, { onEvent: event => events.push(event) })
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const calls = Array.from({ length: 9 }, () => channel.request('write_state'))
  const completed = Promise.allSettled(calls)
  try {
    t.mock.timers.tick(59000)
    app.reply('1', { read: 1 })
    await settle()
    assert.equal(app.requests.length, 9)
    for (let id = 2; id <= 8; id++) app.reply(String(id), { read: id })
    await settle()
    t.mock.timers.tick(1000)
    assert.equal(app.open(), 1, 'the old queue deadline was cleared on dispatch')
    assert.equal(channel.idle(), false)
    calls.push(channel.request('directory'))
    const finalRead = Promise.allSettled([calls[9]])
    t.mock.timers.tick(59000)
    const settled = await completed
    assert.equal(settled.slice(0, 8).every(item => item.status === 'fulfilled'), true)
    assert.equal(settled[8].status, 'rejected')
    assert.equal((await finalRead)[0].status, 'rejected')
    assert.equal(app.requests.length, 10, 'timed-out read is never replayed')
    assert.equal(app.open(), 0)
    assert.equal(channel.idle(), true)
    assert.deepEqual(events, [])
  } finally { channel.close(); await completed; t.mock.timers.reset() }
})

test('pending model and write calls do not hold the read budget', async () => {
  const app = await harness({ holdRequests: true })
  const channel = await app.connect()
  const calls = [channel.request('provider_models'), channel.request('rpc', { method: 'send_message' }), ...Array.from({ length: 8 }, () => channel.request('write_state'))]
  const completed = Promise.allSettled(calls)
  try { assert.equal(app.requests.length, 10) }
  finally { channel.close(); await completed }
})

test('a late timed-out response is not delivered as a workspace push', async t => {
  const app = await harness({ holdRequests: true })
  const events: unknown[] = []
  const channel = await app.connect(undefined, { onEvent: event => events.push(event) })
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const call = channel.request('provider_models')
  const rejected = assert.rejects(call, /还没确认结果/)
  try {
    t.mock.timers.tick(60000)
    await rejected
    assert.equal(app.open(), 1)
    app.reply('1', { models: [] })
    await settle()
    assert.deepEqual(events, [])
    assert.equal(channel.idle(), true)
  } finally { channel.close(); await rejected; t.mock.timers.reset() }
})
