import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { createTransport } from '@vibeface/client/lib/transport.ts'
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

/** A fake Gateway that answers as `serving` (normally the target) or refuses with a text frame. */
async function harness({ local = false, refuse = '', impostor = false } = {}) {
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
  const secure: SecureApi = wasm
  const runtime = createTransport(secure, {
    load: async () => ({ deviceId: browser.deviceId }), remove: async () => {},
    sign: async (message) => hex(await crypto.subtle.sign({ name: 'Ed25519' }, browser.privateKey, new Uint8Array(message))),
  })
  const address = 'https://gateway.example'
  return {
    target: target.deviceId, urls, texts, open: () => open,
    connect: () => runtime.connectDevice({ address, device_id: target.deviceId, ...(local ? { local: true } : {}) }, {}),
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

test('the device key cannot be exported', async () => {
  const { privateKey } = await key()
  assert.equal(privateKey.extractable, false)
  await assert.rejects(crypto.subtle.exportKey('pkcs8', privateKey))
  await assert.rejects(crypto.subtle.exportKey('jwk', privateKey))
})
