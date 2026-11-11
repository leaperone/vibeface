import { createServer, type Server } from 'node:http'
import { once } from 'node:events'
import { createHash } from 'node:crypto'
import { symlink } from 'node:fs/promises'
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import type { Duplex } from 'node:stream'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CLIENT_IPC, CLIENT_ORIGIN, CLIENT_URL } from '../src/shared/client-contract.js'
import type { Inspect } from '../src/main/control-types.js'
import { MAX_HTTP_BODY } from '../src/main/limits.js'

const mocks = vi.hoisted(() => ({
  handlers: new Map<string, (event: unknown, ...args: unknown[]) => unknown>(),
  protocolHandler: null as null | ((request: Request) => Promise<Response>),
  runControl: vi.fn(),
}))

vi.mock('../src/main/control-client.js', () => ({ runControl: mocks.runControl }))

vi.mock('electron', () => ({
  ipcMain: {
    handle(channel: string, callback: (event: unknown, ...args: unknown[]) => unknown) { mocks.handlers.set(channel, callback) },
    removeHandler(channel: string) { mocks.handlers.delete(channel) },
  },
  protocol: {
    handle(_scheme: string, callback: (request: Request) => Promise<Response>) { mocks.protocolHandler = callback },
  },
}))

import { createClientHost } from '../src/main/client-host.js'

const nodeId = 'a'.repeat(64)
const clientId = 'b'.repeat(64)
const capability = 'c'.repeat(64)
const localToken = 'd'.repeat(64)
const cookieValue = 'e'.repeat(64)
let server: Server | undefined
let rootDir: string | undefined
const upgradeSockets = new Set<Duplex>()

type Fixture = { server: Server; origin: string; requests: { path: string; headers: Record<string, string | string[] | undefined>; body: string }[] }

async function localFixture(handler: (request: { path: string; headers: Record<string, string | string[] | undefined>; body: string }, response: import('node:http').ServerResponse) => void): Promise<Fixture> {
  const requests: Fixture['requests'] = []
  const instance = createServer((request, response) => {
    const chunks: Buffer[] = []
    request.on('data', chunk => chunks.push(Buffer.from(chunk)))
    request.on('end', () => {
      const entry = { path: request.url ?? '', headers: request.headers, body: Buffer.concat(chunks).toString('utf8') }
      requests.push(entry)
      handler(entry, response)
    })
  })
  server = instance
  instance.listen(0, '127.0.0.1')
  await once(instance, 'listening')
  const address = instance.address()
  if (!address || typeof address === 'string') throw new Error('HTTP fixture did not bind')
  return { server: instance, origin: `http://127.0.0.1:${address.port}`, requests }
}

function inspected(origin: string): { inspect: Inspect; access: { kind: 'local'; url: string; deviceId: string } } {
  const port = new URL(origin).port
  return {
    inspect: {
      status: 'running', deviceId: nodeId, dataDir: '/fixture/node', localListen: `127.0.0.1:${port}`, listen: '127.0.0.1:4790',
      webEnabled: true, coreVersion: 'fixture', error: null,
    },
    access: { kind: 'local', url: origin, deviceId: nodeId },
  }
}

function contents(): { value: unknown; frame: object; event(url?: string): { sender: unknown; senderFrame: object } } {
  const frame = { url: CLIENT_URL, parent: null }
  const value = { mainFrame: frame, once: vi.fn(), getURL: () => CLIENT_URL }
  return { value, frame, event: (url = CLIENT_URL) => ({ sender: value, senderFrame: { url, parent: null } }) }
}

function trustedEvent(value: unknown, frame: object, url = CLIENT_URL) {
  const senderFrame = frame === (value as { mainFrame: object }).mainFrame ? frame : { url, parent: null }
  return { sender: value, senderFrame }
}

async function invoke(channel: string, event: unknown, ...args: unknown[]): Promise<unknown> {
  const handler = mocks.handlers.get(channel)
  if (!handler) throw new Error(`No handler: ${channel}`)
  return await handler(event, ...args)
}

function json(response: import('node:http').ServerResponse, status: number, value: unknown): void {
  response.writeHead(status, { 'content-type': 'application/json' })
  response.end(JSON.stringify(value))
}

const fixtureCsp = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self' https: ws: wss; object-src 'none'"

async function writeClientAssetFixture(base: string, indexBody = '<!doctype html><title>vibeface-app</title>'): Promise<void> {
  await writeFile(join(base, '.vibeface-build.json'), JSON.stringify({ version: 1, component: 'web', content_security_policy: fixtureCsp }))
  await writeFile(join(base, 'index.html'), indexBody)
}

async function protocolHandler(runtimeRoot: string): Promise<(request: Request) => Promise<Response>> {
  const host = createClientHost({ runtimeRoot, resolveSelectedNode: async () => null, getClientContents: () => null, showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core' })
  host.registerProtocol()
  const handler = mocks.protocolHandler
  if (!handler) throw new Error('protocol handler missing')
  return handler
}

beforeEach(() => { mocks.handlers.clear(); mocks.protocolHandler = null; mocks.runControl.mockReset() })
afterEach(async () => {
  for (const socket of upgradeSockets) socket.destroy()
  upgradeSockets.clear()
  if (server) await new Promise<void>(resolve => server?.close(() => resolve()))
  server = undefined
  if (rootDir) await rm(rootDir, { recursive: true, force: true })
  rootDir = undefined
})

describe('Desktop client host', () => {
  it('serves only confined GET/HEAD assets with CSP from the signed app receipt', async () => {
    rootDir = await mkdtemp(join(tmpdir(), 'vibeface-client-assets-'))
    await mkdir(join(rootDir, 'assets'))
    await writeClientAssetFixture(rootDir, '<!doctype html>fixture')
    await writeFile(join(rootDir, 'assets', 'crypto.wasm'), Buffer.from([0, 97, 115, 109]))
    const handler = await protocolHandler(rootDir)
    const wasm = await handler(new Request(`${CLIENT_ORIGIN}/assets/crypto.wasm`))
    expect(wasm.status).toBe(200)
    expect(wasm.headers.get('content-type')).toBe('application/wasm')
    expect(wasm.headers.get('content-security-policy')).toContain(`${CLIENT_ORIGIN}`)
    expect(wasm.headers.get('content-security-policy')).toContain("'wasm-unsafe-eval'")
    expect((await handler(new Request(`${CLIENT_ORIGIN}/%2e%2e/.vibeface-build.json`))).status).toBe(403)
    expect((await handler(new Request(`https://remote.example/`))).status).toBe(404)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/`, { method: 'POST' }))).status).toBe(404)
  })

  it('does not make local requests or initialize a node when no verified selection exists', async () => {
    let resolutions = 0
    const window = contents()
    const host = createClientHost({
      runtimeRoot: '/unused', getClientContents: () => window.value as never,
      resolveSelectedNode: async () => { resolutions += 1; return null }, showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core',
    })
    host.installIpc()
    const setup = await invoke(CLIENT_IPC.localSetup, trustedEvent(window.value, window.frame))
    expect(setup).toBeNull()
    expect(resolutions).toBe(1)
    expect(mocks.handlers.size).toBeGreaterThan(0)
    const subframe = { sender: window.value, senderFrame: { url: CLIENT_URL, parent: {} } }
    await expect(invoke(CLIENT_IPC.localSetup, subframe)).rejects.toThrow('Untrusted client window')
    host.dispose()
  })

  it('keeps the inspected loopback origin, bootstrap capability, and exchange cookie inside the intended boundary', async () => {
    const fixture = await localFixture((request, response) => {
      expect(request.headers.host).toBe(new URL(fixture.origin).host)
      expect(request.headers.origin).toBe(fixture.origin)
      if (request.path === '/api/local/setup') json(response, 200, { kind: 'uninitialized', invitation: { url: fixture.origin, device_id: nodeId, capability } })
      else if (request.path === '/api/local/exchange') {
        expect(JSON.parse(request.body)).toEqual({ token: localToken, node_id: nodeId, device_id: clientId })
        response.writeHead(200, { 'content-type': 'application/json', 'set-cookie': `vibeface_local_${nodeId}=${cookieValue}; HttpOnly; SameSite=Strict; Path=/api/local; Max-Age=86400` })
        response.end(JSON.stringify({ network_id: 'f'.repeat(64), entry: { address: fixture.origin, device_id: nodeId, local: true } }))
      } else if (request.path === '/api/local/info') {
        expect(request.headers.cookie).toBe(`vibeface_local_${nodeId}=${cookieValue}`)
        json(response, 200, { enabled: true, has_session: true, device_id: clientId, entry: { address: fixture.origin, device_id: nodeId, local: true } })
      } else json(response, 404, {})
    })
    mocks.runControl.mockResolvedValue({ kind: 'uninitialized', invitation: { url: fixture.origin, device_id: nodeId, capability } })
    const window = contents()
    const host = createClientHost({
      runtimeRoot: '/unused', getClientContents: () => window.value as never, resolveSelectedNode: async () => inspected(fixture.origin),
      showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core',
    })
    host.installIpc()
    const call = trustedEvent(window.value, window.frame)
    const setup = await invoke(CLIENT_IPC.localSetup, call) as { kind: string; entry: { address: string; kind: string }; capability: string }
    expect(setup).toEqual({ kind: 'uninitialized', entry: { kind: 'gateway', address: fixture.origin, device_id: nodeId }, capability })
    expect(mocks.runControl).toHaveBeenCalledWith({ command: '/unused/core', args: ['--control'] }, { command: 'local_setup', data_dir: '/fixture/node' })
    expect(JSON.stringify(setup)).not.toContain(cookieValue)
    const launch = { token: localToken, deviceId: nodeId, rejoin: false }
    const exchanged = await invoke(CLIENT_IPC.localEntry, call, clientId, launch)
    expect(exchanged).toEqual({ kind: 'local', address: fixture.origin, device_id: nodeId })
    expect(JSON.stringify(exchanged)).not.toContain(cookieValue)
    const resumed = await invoke(CLIENT_IPC.localEntry, call, clientId, null)
    expect(resumed).toEqual(exchanged)
    expect(fixture.requests.map(item => item.path)).toEqual(['/api/local/exchange', '/api/local/info'])
    host.dispose()
  })

  it('rejects wrong-node setup, expired local tickets, redirects, and foreign frames', async () => {
    const fixture = await localFixture((request, response) => {
      if (request.path === '/api/local/setup') json(response, 200, { kind: 'uninitialized', invitation: { url: fixture.origin, device_id: '9'.repeat(64), capability } })
      else if (request.path === '/api/local/exchange') json(response, 401, { error: 'expired' })
      else response.writeHead(302, { location: 'https://remote.example/' }).end()
    })
    mocks.runControl.mockResolvedValue({ kind: 'uninitialized', invitation: { url: fixture.origin, device_id: '9'.repeat(64), capability } })
    const window = contents()
    const host = createClientHost({
      runtimeRoot: '/unused', getClientContents: () => window.value as never, resolveSelectedNode: async () => inspected(fixture.origin),
      showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core',
    })
    host.installIpc()
    const call = trustedEvent(window.value, window.frame)
    await expect(invoke(CLIENT_IPC.localSetup, call)).rejects.toThrow('Local setup belongs to another Node')
    await expect(invoke(CLIENT_IPC.localEntry, call, clientId, { token: localToken, deviceId: nodeId, rejoin: false })).rejects.toThrow('expired or was already used')
    await expect(invoke(CLIENT_IPC.localEntry, call, clientId, { token: localToken, deviceId: '9'.repeat(64), rejoin: false })).rejects.toThrow('another Node')
    const foreign = { sender: window.value, senderFrame: { url: 'https://remote.example/', parent: null } }
    await expect(invoke(CLIENT_IPC.localSetup, foreign)).rejects.toThrow('Untrusted client window')
    host.dispose()
  })

  it('rejects a local Node HTTP body that exceeds the shared size cap', async () => {
    const fixture = await localFixture((request, response) => {
      if (request.path === '/api/local/exchange') {
        response.writeHead(200, { 'content-type': 'application/json' })
        response.end('x'.repeat(MAX_HTTP_BODY + 1))
      } else json(response, 404, {})
    })
    const window = contents()
    const host = createClientHost({
      runtimeRoot: '/unused', getClientContents: () => window.value as never, resolveSelectedNode: async () => inspected(fixture.origin),
      showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core',
    })
    host.installIpc()
    const call = trustedEvent(window.value, window.frame)
    await expect(invoke(CLIENT_IPC.localEntry, call, clientId, { token: localToken, deviceId: nodeId, rejoin: false })).rejects.toThrow('Local response too large')
    host.dispose()
  })

  it('uses the selected local Node normal gateway channel for bootstrap with one fixed connect frame and no cookie', async () => {
    let connectFrames = 0
    const fixture = await localFixture((_request, response) => json(response, 404, {}))
    fixture.server.on('upgrade', (request, socket) => {
      upgradeSockets.add(socket)
      socket.on('close', () => upgradeSockets.delete(socket))
      expect(request.url).toBe('/api/client')
      expect(request.headers.host).toBe(new URL(fixture.origin).host)
      expect(request.headers.origin).toBe(fixture.origin)
      expect(request.headers.cookie).toBeUndefined()
      const key = request.headers['sec-websocket-key']
      if (typeof key !== 'string') throw new Error('WebSocket key missing')
      const accept = createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64')
      socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
      let incoming = Buffer.alloc(0)
      socket.on('data', (raw: Buffer) => {
        incoming = Buffer.concat([incoming, raw])
        while (incoming.length >= 2) {
          const first = incoming[0] ?? 0
          const headerLength = (incoming[1] ?? 0) & 0x7f
          let length = headerLength
          let maskOffset = 2
          if (headerLength === 126) { if (incoming.length < 4) return; length = incoming.readUInt16BE(2); maskOffset = 4 }
          if (incoming.length < maskOffset + 4 + length) return
          const mask = incoming.subarray(maskOffset, maskOffset + 4)
          const data = Buffer.from(incoming.subarray(maskOffset + 4, maskOffset + 4 + length))
          incoming = incoming.subarray(maskOffset + 4 + length)
          for (let index = 0; index < data.length; index += 1) data[index] = (data[index] ?? 0) ^ (mask[index % 4] ?? 0)
          if (first === 0x81) {
            connectFrames += 1
            expect(data.toString('utf8')).toBe(JSON.stringify({ type: 'connect', target: nodeId }))
            continue
          }
          socket.write(Buffer.concat([Buffer.from([0x82, data.length]), data]))
        }
      })
    })
    const window = contents()
    const host = createClientHost({
      runtimeRoot: '/unused', getClientContents: () => window.value as never, resolveSelectedNode: async () => inspected(fixture.origin),
      showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core',
    })
    host.installIpc()
    const call = trustedEvent(window.value, window.frame)
    const streamId = await invoke(CLIENT_IPC.gatewayOpen, call, nodeId, fixture.origin, 1000) as string
    const frameBytes = new Uint8Array([2, 4, 6, 8])
    await invoke(CLIENT_IPC.carrierSend, call, streamId, frameBytes)
    const echoed = await invoke(CLIENT_IPC.carrierReceive, call, streamId) as { data: Uint8Array }
    expect(connectFrames).toBe(1)
    expect([...echoed.data]).toEqual([...frameBytes])
    expect(fixture.requests).toEqual([])
    await invoke(CLIENT_IPC.carrierClose, call, streamId)
    host.dispose()
  })

  it('passes WebSocket frame bytes unchanged, keeps cookies out of IPC, and closes channels without replay', async () => {
    let received: Buffer | null = null
    const fixture = await localFixture((request, response) => {
      if (request.path === '/api/local/exchange') {
        response.writeHead(200, { 'content-type': 'application/json', 'set-cookie': `vibeface_local_${nodeId}=${cookieValue}; HttpOnly; SameSite=Strict; Path=/api/local` })
        response.end(JSON.stringify({ network_id: 'f'.repeat(64), entry: { address: fixture.origin, device_id: nodeId, local: true } }))
      } else json(response, 404, {})
    })
    fixture.server.on('upgrade', (request, socket) => {
      upgradeSockets.add(socket)
      socket.on('close', () => upgradeSockets.delete(socket))
      expect(request.url).toBe('/api/local/ws')
      expect(request.headers.host).toBe(new URL(fixture.origin).host)
      expect(request.headers.origin).toBe(fixture.origin)
      expect(request.headers.cookie).toBe(`vibeface_local_${nodeId}=${cookieValue}`)
      const key = request.headers['sec-websocket-key']
      if (typeof key !== 'string') throw new Error('WebSocket key missing')
      const accept = createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64')
      socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
      socket.on('data', (raw: Buffer) => {
        const length = (raw[1] ?? 0) & 0x7f
        const maskOffset = length < 126 ? 2 : 4
        const mask = raw.subarray(maskOffset, maskOffset + 4)
        const data = Buffer.from(raw.subarray(maskOffset + 4))
        for (let index = 0; index < data.length; index += 1) data[index] = (data[index] ?? 0) ^ (mask[index % 4] ?? 0)
        received = data
        const head = Buffer.from([0x82, data.length])
        socket.write(Buffer.concat([head, data]))
      })
    })
    const window = contents()
    const host = createClientHost({
      runtimeRoot: '/unused', getClientContents: () => window.value as never, resolveSelectedNode: async () => inspected(fixture.origin),
      showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core',
    })
    host.installIpc()
    const call = trustedEvent(window.value, window.frame)
    await invoke(CLIENT_IPC.localEntry, call, clientId, { token: localToken, deviceId: nodeId, rejoin: false })
    const streamId = await invoke(CLIENT_IPC.carrierOpen, call, nodeId, 1000) as string
    const sent = new Uint8Array([3, 5, 8, 13, 21])
    await invoke(CLIENT_IPC.carrierSend, call, streamId, sent)
    const frame = await invoke(CLIENT_IPC.carrierReceive, call, streamId) as { text: boolean; data: Uint8Array }
    expect([...frame.data]).toEqual([...sent])
    expect(frame.text).toBe(false)
    expect(received).toEqual(Buffer.from(sent))
    expect(streamId).not.toContain(cookieValue)
    await invoke(CLIENT_IPC.carrierClose, call, streamId)
    await expect(invoke(CLIENT_IPC.carrierSend, call, streamId, sent)).rejects.toThrow('closed')
    const second = await invoke(CLIENT_IPC.carrierOpen, call, nodeId, 1000)
    host.disconnect()
    await expect(invoke(CLIENT_IPC.carrierReceive, call, second)).rejects.toThrow('closed')
    await expect(invoke(CLIENT_IPC.carrierOpen, call, nodeId, 1000)).rejects.toThrow('A local session is required')
    await invoke(CLIENT_IPC.localEntry, call, clientId, { token: localToken, deviceId: nodeId, rejoin: false })
    const refreshed = await invoke(CLIENT_IPC.carrierOpen, call, nodeId, 1000)
    expect(refreshed).not.toBe(second)
    host.dispose()
  })

  it('maps this Node public invitation origins onto its pinned private Gateway listener', async () => {
    const fixture = await localFixture((_request, response) => json(response, 404, {}))
    const connects: unknown[] = []
    fixture.server.on('upgrade', (request, socket) => {
      upgradeSockets.add(socket)
      socket.on('close', () => upgradeSockets.delete(socket))
      expect(request.url).toBe('/api/client')
      expect(request.headers.host).toBe(new URL(fixture.origin).host)
      expect(request.headers.origin).toBe(fixture.origin)
      expect(request.headers.cookie).toBeUndefined()
      const key = request.headers['sec-websocket-key']
      if (typeof key !== 'string') throw new Error('Gateway key missing')
      const accept = createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64')
      socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
      socket.once('data', (raw: Buffer) => {
        const length = (raw[1] ?? 0) & 0x7f
        const mask = raw.subarray(2, 6), bytes = Buffer.from(raw.subarray(6, 6 + length))
        for (let i = 0; i < bytes.length; i++) bytes[i] = (bytes[i] ?? 0) ^ (mask[i % 4] ?? 0)
        connects.push(JSON.parse(bytes.toString('utf8')))
      })
    })
    const selected = inspected(fixture.origin)
    selected.inspect.listen = '127.0.0.1:30479'
    const window = contents()
    const host = createClientHost({ runtimeRoot: '/unused', getClientContents: () => window.value as never,
      resolveSelectedNode: async () => selected, showNodePanel: () => {}, clientBridgeProgram: () => '/unused/core' })
    host.installIpc()
    const call = trustedEvent(window.value, window.frame)
    for (const address of ['http://localhost:30479', 'http://127.0.0.1:30479']) {
      const stream = await invoke(CLIENT_IPC.gatewayOpen, call, nodeId, address, 1000)
      expect(typeof stream).toBe('string')
      await invoke(CLIENT_IPC.carrierClose, call, stream)
    }
    await expect.poll(() => connects.length).toBe(2)
    expect(connects).toEqual([{ type: 'connect', target: nodeId }, { type: 'connect', target: nodeId }])
    expect(await invoke(CLIENT_IPC.gatewayOpen, call, nodeId, 'http://127.0.0.1:30480', 1000)).toBeNull()
    expect(await invoke(CLIENT_IPC.gatewayOpen, call, nodeId, 'http://192.168.1.1:30479', 1000)).toBeNull()
    expect(await invoke(CLIENT_IPC.gatewayOpen, call, 'e'.repeat(64), 'http://127.0.0.1:30479', 1000)).toBeNull()
    host.dispose()
  })

  it('serves the signed SPA shell for client routes and preserves 404 for APIs and missing assets', async () => {
    rootDir = await mkdtemp(join(tmpdir(), 'vibeface-client-assets-'))
    await writeClientAssetFixture(rootDir)
    const handler = await protocolHandler(rootDir)
    const shell = await handler(new Request(`${CLIENT_ORIGIN}/`))
    expect(shell.status).toBe(200)
    expect(shell.headers.get('content-type')).toBe('text/html; charset=utf-8')
    expect(shell.headers.get('cache-control')).toBe('no-store')
    expect(await shell.text()).toContain('vibeface-app')
    for (const path of ['/join', '/tasks', '/settings/desktop-node', '/devices', `/b/${'a'.repeat(32)}`, '/b/room/work/item']) {
      const route = await handler(new Request(`${CLIENT_ORIGIN}${path}`))
      expect(route.status).toBe(200)
      expect(route.headers.get('content-type')).toBe('text/html; charset=utf-8')
      expect(route.headers.get('content-security-policy')).toContain(CLIENT_ORIGIN)
      expect(await route.text()).toContain('vibeface-app')
    }
    const head = await handler(new Request(`${CLIENT_ORIGIN}/settings/desktop-node`, { method: 'HEAD' }))
    expect(head.status).toBe(200)
    expect(head.headers.get('content-length')).toBe(String(Buffer.byteLength('<!doctype html><title>vibeface-app</title>')))
    expect(await head.text()).toBe('')
    expect((await handler(new Request(`${CLIENT_ORIGIN}/api/local/setup`))).status).toBe(404)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/assets/missing.js`))).status).toBe(404)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/unknown/route`))).status).toBe(404)
  })

  it('rejects traversal, encoded traversal, and symlinks outside the signed asset root', async () => {
    rootDir = await mkdtemp(join(tmpdir(), 'vibeface-client-assets-'))
    await writeClientAssetFixture(rootDir)
    const outside = await mkdtemp(join(tmpdir(), 'vibeface-client-outside-'))
    await writeFile(join(outside, 'secret.txt'), 'leak')
    await symlink(join(outside, 'secret.txt'), join(rootDir, 'escape-link'))
    const handler = await protocolHandler(rootDir)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/../.vibeface-build.json`))).status).toBe(403)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/%2e%2e/.vibeface-build.json`))).status).toBe(403)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/escape-link`))).status).toBe(403)
    expect((await handler(new Request(`${CLIENT_ORIGIN}/.vibeface-build.json`))).status).toBe(403)
    await rm(outside, { recursive: true, force: true })
  })
})
