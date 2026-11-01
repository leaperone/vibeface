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

/** A fake Gateway, or with `lan` a Node on the LAN, that answers as `serving` (normally the target) or refuses with a text frame. */
async function harness({ local = false, lan = false, refuse = '', impostor = false } = {}) {
  await initCrypto({ module_or_path: readFileSync(new URL('../../generated/vibeface_secure_bg.wasm', import.meta.url)) })
  const browser = await key(), target = await key(), serving = impostor ? await key() : target
  const urls: string[] = [], texts: string[] = []
  let open = 0
  class GatewaySocket extends EventTarget {
    static OPEN = 1
    readyState = 0
    binaryType = 'arraybuffer'
    handshake: Handshake | undefined
    channel: ReturnType<Handshake['finish']> | undefined
    dead = false
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
    send(value: string | Uint8Array) {
      if (this.dead) return
      if (typeof value === 'string') {
        texts.push(value)
        if (refuse) { this.emit(JSON.stringify({ type: 'error', error: refuse })); this.handshake?.free(); this.handshake = undefined }
        return
      }
      if (this.handshake) {
        this.handshake.read(value)
        if (this.handshake.isFinished()) { this.channel = this.handshake.finish(); this.handshake.free(); this.handshake = undefined }
        else this.emit(new Uint8Array(this.handshake.write(new Uint8Array())).buffer)
      } else if (this.channel) {
        const request = this.channel.openRecord(value)
        if (!request) return
        const { id } = JSON.parse(new TextDecoder().decode(request))
        for (const frame of this.channel.sealMessage(new TextEncoder().encode(JSON.stringify({ id, result: { device_id: serving.deviceId } })))) this.emit(new Uint8Array(frame).buffer)
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
  const runtime = createTransport(secure, {
    load: async () => ({ deviceId: browser.deviceId }), remove: async () => {},
    sign: async (message) => hex(await crypto.subtle.sign({ name: 'Ed25519' }, browser.privateKey, new Uint8Array(message))),
  }, lan ? dial : undefined)
  const address = 'https://gateway.example', device_id = target.deviceId
  const entry: ConnectionEntry = lan ? { kind: 'lan', addrs: ['192.168.1.4:7001'], device_id } : { kind: local ? 'local' : 'gateway', address, device_id }
  return {
    target: target.deviceId, urls, texts, dials, open: () => open,
    connect: (to = entry) => runtime.connectDevice(to, {}),
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

test('a local session uses the cookie endpoint and sends no connect frame', async () => {
  const app = await harness({ local: true })
  const channel = await app.connect()
  assert.deepEqual(app.urls, ['wss://gateway.example/api/local/ws'])
  assert.deepEqual(app.texts, [])
  channel.close()
})

test('the app reaches a Node on its LAN directly: no Gateway, no connect frame, and Noise still proves the device', async () => {
  const app = await harness({ lan: true })
  const channel = await app.connect()
  assert.deepEqual(app.dials, [{ deviceId: app.target, addrs: ['192.168.1.4:7001'] }])
  assert.deepEqual([app.urls, app.texts], [['quic://192.168.1.4:7001'], []])
  assert.equal((await channel.request('info') as { device_id: string }).device_id, app.target)
  channel.close(); await settle()
  assert.equal(app.open(), 0)
  await assert.rejects(app.connect({ kind: 'lan', addrs: ['192.168.1.99:7001'], device_id: app.target }), /在局域网里连不上这台电脑/)
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

test("the app's join QR code, a bare ticket and the web link name the same ticket", () => {
  const link = 'https://node.example/join#vibeface1test', web = { ticket: 'vibeface1test', origin: 'https://node.example' }
  assert.deepEqual(joinTicket(link), web)
  assert.deepEqual(joinTicket(`vibeface://join?link=${encodeURIComponent(link)}`), web)
  assert.deepEqual(joinTicket('vibeface://join?link=vibeface1test'), { ticket: 'vibeface1test', origin: null })
  assert.deepEqual(joinTicket(' vibeface1test '), { ticket: 'vibeface1test', origin: null })
  assert.equal(joinTicket(`vibeface://join?link=${encodeURIComponent('vibeface://join?link=vibeface1test')}`), null)
  assert.equal(joinTicket('https://node.example/#vibeface1test'), null)
})

test('the device key cannot be exported', async () => {
  const { privateKey } = await key()
  assert.equal(privateKey.extractable, false)
  await assert.rejects(crypto.subtle.exportKey('pkcs8', privateKey))
  await assert.rejects(crypto.subtle.exportKey('jwk', privateKey))
})
