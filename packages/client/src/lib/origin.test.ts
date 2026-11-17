import assert from 'node:assert/strict'
import test from 'node:test'
import { parseHttpOrigin, parseHttpsOrigin } from './origin.ts'
import { APP_ORIGIN, PUBLIC_GATEWAY, candidates, defaultGateway, gatewaysOf, isPublicUrl, markReached, openEntrances, storedEntrances, storedGateway, verifiedEntrances, type Entrance } from './entrances.ts'
import { parseEntry, type MembershipView } from './node.ts'
import { createTransport, lanAddrs, validEndpoint, type DeviceKeys } from './transport.ts'
import type { SecureApi } from '../secure-api.ts'

const a = 'a'.repeat(64), b = 'b'.repeat(64), c = 'c'.repeat(64), d = 'd'.repeat(64)
const member = (device_id: string, kind: 'node' | 'client' = 'node') => ({ device_id, kind, name: 'Node', manage: false })
const view: MembershipView = { network_id: '1'.repeat(64), epoch: 1, recovery_key: '2'.repeat(64), heads: [], members: [member(a), member(b), member(c), member(d, 'client')], revoked: [{ device_id: c, cutoff_seq: 0, cutoff_hash: null }], attesters: [], pending: null, stopped: [] }
const signed = (gateway_url: string | null, changes: Record<string, unknown> = {}) => ({ payload: JSON.stringify({ version: 1, network_id: view.network_id, device_id: a, seq: 1, endpoint: { addrs: [] }, gateway_url, ...changes }), signature: 'signature' })
const cached = (changes: Partial<Entrance> = {}): Entrance => ({ device_id: a, seq: 1, gateway_url: 'https://node.example', public: true, direct: [], last_ok_ms: 0, ...changes })

// Exercise each public boundary with the same hostile strings, rather than recreating
// the parser's implementation as an expected-result oracle.
const invalidOrigins = [
  'https://:secret@node.example', 'https://user@node.example', 'https://user:secret@node.example', 'https://@node.example',
  'https://node.example/path', 'https://node.example/?query', 'https://node.example/#fragment', 'https://node.example?', 'https://node.example#',
  'https://node.example/.', 'https://node.example/a/..', 'https://node.example/%2e', 'https://node.example\\',
  ' https://node.example', 'https://node.example\n', 'https://node.example\0', 'https://node.example\u001f', 'https://node.example\u007f', 'https://no\tde.example', 'https:node.example', '//node.example',
  'ws://node.example', 'ftp://node.example', 'file:///node.example', 'vibeface://node.example', 'https://',
  'http://999.999.999.999', 'http://[not-ipv6]', 'https://node.example:65536', 'https://node.example:/',
]

function page(origin: string | undefined, run: () => void) {
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'location')
  if (origin === undefined) Reflect.deleteProperty(globalThis, 'location')
  else Object.defineProperty(globalThis, 'location', { configurable: true, value: { origin, protocol: new URL(origin).protocol } })
  try { run() } finally {
    if (previous) Object.defineProperty(globalThis, 'location', previous)
    else Reflect.deleteProperty(globalThis, 'location')
  }
}

test('HTTP origin syntax consistently accepts root slash/default ports and derives canonical dial origins', () => {
  for (const [input, origin] of [
    ['HTTPS://NODE.EXAMPLE:443/', 'https://node.example'], ['http://node.example:80/', 'http://node.example'],
    ['https://node.example', 'https://node.example'], ['http://192.168.1.4:4790/', 'http://192.168.1.4:4790'],
    ['http://[fd00::1]:4790/', 'http://[fd00::1]:4790'], ['https://node.example:4790', 'https://node.example:4790'],
  ]) assert.equal(parseHttpOrigin(input)?.origin, origin, input)
  assert.equal(parseHttpsOrigin('https://node.example:443/')?.origin, 'https://node.example')
  assert.equal(parseHttpsOrigin('http://node.example'), null)
  for (const value of [...invalidOrigins, null, undefined, 1, {}, []]) assert.equal(parseHttpOrigin(value), null, String(value))
})

test('signed/cache/gateway/ConnectionEntry boundaries all reject credentials, nonroot paths, queries, fragments and URL repairs', () => {
  page(undefined, () => {
    for (const value of invalidOrigins) {
      assert.throws(() => validEndpoint(value), value)
      assert.throws(() => parseEntry({ device_id: a, address: value }), value)
      assert.equal(storedGateway(value), null, value)
      assert.deepEqual(verifiedEntrances([signed(value)], view, () => true), [], value)
      assert.deepEqual(storedEntrances([cached({ gateway_url: value })]), [], value)
      assert.deepEqual(gatewaysOf([{ device_id: a, url: value }]), [], value)
      assert.equal(isPublicUrl(value), false, value)
    }
  })
})

test('connection policy keeps HTTP pages/native LAN separate from HTTPS browser restrictions', () => {
  for (const origin of [undefined, 'http://page.example']) page(origin, () => {
    assert.equal(validEndpoint('http://192.168.1.4:4790/'), 'http://192.168.1.4:4790')
    assert.equal(parseEntry({ device_id: a, address: 'http://node.example:80/' }).kind, 'gateway')
  })
  page('https://page.example', () => {
    for (const address of ['http://node.example', 'http://192.168.1.4', 'http://[fd00::1]']) {
      assert.throws(() => validEndpoint(address), /HTTPS 页面/)
      assert.throws(() => parseEntry({ device_id: a, address }), /HTTPS 页面/)
      assert.equal(storedGateway(address), null)
    }
    for (const address of ['http://localhost:4790', 'http://127.0.0.1:4790', 'http://[::1]:4790']) assert.equal(validEndpoint(address), address)
    assert.equal(validEndpoint('https://192.168.1.4:443/'), 'https://192.168.1.4')
    // Discovery/cache retain valid HTTP descriptors even when this browser cannot dial them.
    const local = verifiedEntrances([signed('http://192.168.1.4:4790/')], view, () => true)
    assert.equal(local[0].public, false)
    assert.deepEqual(storedEntrances(local), local)
    assert.deepEqual(parseEntry({ device_id: a, addrs: ['192.168.1.4:7001'] }), { kind: 'lan', device_id: a, addrs: ['192.168.1.4:7001'] })
  })
  page('http://page.example', () => {
    assert.equal(parseEntry({ device_id: a, address: 'http://page.example:80/', local: true }).kind, 'local')
    assert.throws(() => parseEntry({ device_id: a, address: 'http://other.example', local: true }), /只能连这个节点/)
  })
})

test('page and chosen gateway cache use shared syntax with explicit defaults', () => {
  assert.equal(defaultGateway('http://node.example:80/'), 'http://node.example')
  assert.equal(defaultGateway('https://NODE.EXAMPLE:443/'), 'https://node.example')
  for (const origin of [APP_ORIGIN, `${APP_ORIGIN}:443/`, 'vibeface-app://app', 'null', undefined, ...invalidOrigins]) assert.equal(defaultGateway(origin), PUBLIC_GATEWAY)
  page(undefined, () => assert.equal(storedGateway('HTTPS://NODE.EXAMPLE:443/'), 'https://node.example'))
})

test('signed raw descriptor/payload/signature survive verification and caching; dial projection alone canonicalizes', () => {
  const record = signed('HTTPS://NODE.EXAMPLE:443/')
  const before = structuredClone(record), checked: unknown[] = []
  const found = verifiedEntrances([record], view, (deviceId, payload, signature) => { checked.push([deviceId, payload, signature]); return true })
  assert.deepEqual(checked, [[a, before.payload, before.signature]])
  assert.deepEqual(record, before)
  assert.equal(found[0].gateway_url, 'HTTPS://NODE.EXAMPLE:443/')
  const saved = storedEntrances(JSON.parse(JSON.stringify(found)))
  assert.equal(saved[0].gateway_url, 'HTTPS://NODE.EXAMPLE:443/')
  assert.equal(openEntrances(saved)[0].gateway_url, 'https://node.example')
  const current = { kind: 'gateway' as const, address: 'https://node.example', device_id: a }
  assert.deepEqual(candidates(saved, current), [current], 'the same canonical origin is not added twice')
  const reached = markReached(saved, current, 42)
  assert.equal(reached[0].last_ok_ms, 42)
  assert.equal(reached[0].gateway_url, 'HTTPS://NODE.EXAMPLE:443/')
  assert.equal(saved[0].last_ok_ms, 0)
})

test('verified entrances preserve network/current-member/signature/sequence/withdrawal rules', () => {
  const records = [signed('https://node.example', { seq: 2 }), signed('https://older.example', { seq: 1 }), signed(null, { seq: 3 }), signed('https://stale.example', { seq: 2 })]
  assert.deepEqual(verifiedEntrances(records, view, () => true), [cached({ seq: 3, gateway_url: null, public: false })])
  for (const changes of [{ network_id: b }, { device_id: c }, { device_id: d }, { device_id: 'e'.repeat(64) }, { seq: -1 }, { seq: 0.5 }, { seq: Number.MAX_SAFE_INTEGER + 1 }, { seq: null }, { version: 2 }]) {
    assert.deepEqual(verifiedEntrances([signed('https://node.example', changes)], view, () => true), [], JSON.stringify(changes))
  }
  assert.deepEqual(verifiedEntrances([signed('https://node.example')], view, () => false), [])
  assert.equal(verifiedEntrances([signed('https://node.example', { seq: 0 })], view, () => true)[0].seq, 0)
})

test('cache rejects invalid seq/time, URL/public mismatch and LAN metadata without rewriting intent', () => {
  for (const number of [-1, 0.5, NaN, Infinity, -Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    assert.deepEqual(storedEntrances([cached({ seq: number })]), [], `seq ${number}`)
    assert.deepEqual(storedEntrances([cached({ last_ok_ms: number })]), [], `time ${number}`)
  }
  assert.equal(storedEntrances([cached({ seq: 0, last_ok_ms: Number.MAX_SAFE_INTEGER })]).length, 1)
  for (const item of [null, [], 'junk', { ...cached(), device_id: 'bad' }, { ...cached(), seq: '1' }, { ...cached(), last_ok_ms: '0' }, { ...cached(), direct: undefined }, cached({ gateway_url: null }), cached({ public: false }), cached({ gateway_url: 'http://192.168.1.4', public: true })]) assert.deepEqual(storedEntrances([item]), [])
  const withdrawal = cached({ gateway_url: null, public: false })
  assert.deepEqual(storedEntrances([withdrawal]), [withdrawal])
  const local = cached({ gateway_url: 'http://192.168.1.4:80/', public: false, direct: ['192.168.1.4:7001', '[fd00::1]:7001'] })
  assert.deepEqual(storedEntrances([local]), [local])
  assert.equal(local.gateway_url, 'http://192.168.1.4:80/')
  for (const direct of [['999.1.1.1:7'], ['[:::]:7'], ['10.0.0.1:0'], ['10.0.0.1:65536'], ['10.0.0.1:7', '10.0.0.1:7']]) assert.deepEqual(storedEntrances([cached({ direct })]), [])
})

test('LAN parser keeps real IPv4/IPv6 and filters malformed literal IPs/ports', () => {
  const valid = ['192.168.1.4:7001', '[fd00::1]:7001', '[::1]:65535', '10.0.0.1:1', '[FD00:0:0::A]:7001', '[::ffff:192.168.1.4]:7001']
  const invalid = ['999.999.999.999:80', '256.1.1.1:80', '[:::]:80', '[gggg::1]:80', '192.168.1.4:0', '192.168.1.4:65536', '192.168.001.4:80', 'node.local:80', '127.1:80', 'http://10.0.0.1:80']
  assert.deepEqual(lanAddrs([...valid, ...invalid, valid[0]]), valid)
  const record = signed(null, { endpoint: { addrs: [...valid, ...invalid].map(Ip => ({ Ip })) } })
  assert.deepEqual(verifiedEntrances([record], view, () => true)[0].direct, valid)
  for (const addr of invalid) assert.throws(() => parseEntry({ device_id: a, addrs: [addr] }), /格式无效/)
})

test('public classification and HTTPS-only gateway boundary retain separate private-address policies', () => {
  for (const address of ['https://node.example', 'https://203.0.113.4']) assert.equal(isPublicUrl(address), true)
  for (const address of ['http://node.example', 'https://localhost', 'https://localhost.', 'https://127.0.0.1', 'https://10.0.0.1', 'https://192.168.1.4', 'https://172.16.1.4', 'https://node.local', 'https://node.lan', 'https://node.internal', 'https://node.home.arpa', 'https://[fd00::1]', 'https://[fe80::1]', 'https://[::1]']) assert.equal(isPublicUrl(address), false, address)
  const gateways = gatewaysOf([{ device_id: a, url: 'HTTPS://NODE.EXAMPLE:443/' }, { device_id: b, url: 'https://node.example' }, { device_id: a, url: 'https://192.168.1.4/' }, { device_id: a, url: 'http://node.example' }, { device_id: 'bad', url: 'https://other.example' }])
  assert.deepEqual(gateways, [{ device_id: b, url: 'https://node.example' }, { device_id: a, url: 'https://192.168.1.4' }])
  assert.deepEqual(gatewaysOf(JSON.parse(JSON.stringify(gateways))), gateways)
  assert.equal(gatewaysOf(Array.from({ length: 17 }, (_, index) => ({ device_id: a, url: `https://node${index}.example` }))).length, 16)
})

test('transport revalidates unparsed entries before any browser socket or native carrier', async () => {
  const previousLocation = Object.getOwnPropertyDescriptor(globalThis, 'location'), previousSocket = Object.getOwnPropertyDescriptor(globalThis, 'WebSocket')
  const sockets: string[] = [], native: string[] = [], nativeLocal: string[] = [], dialed: string[][] = []
  const reached = new Error('carrier reached')
  Object.defineProperty(globalThis, 'WebSocket', { configurable: true, value: class { constructor(url: string) { sockets.push(url); throw reached } } })
  Object.defineProperty(globalThis, 'location', { configurable: true, value: { origin: 'https://page.example', protocol: 'https:' } })
  const keys: DeviceKeys = { load: async () => ({ deviceId: b }), remove: async () => {}, sign: async () => 'signature' }
  const web = createTransport({} as SecureApi, keys)
  const app = createTransport({} as SecureApi, keys, async (_id, addrs) => { dialed.push(addrs); throw reached }, async entry => { nativeLocal.push(entry.address); throw reached }, async entry => { native.push(entry.address); throw reached })
  try {
    for (const address of [...invalidOrigins, 'http://192.168.1.4']) {
      await assert.rejects(web.connectDevice({ kind: 'gateway', address, device_id: a }))
      await assert.rejects(app.connectDevice({ kind: 'gateway', address, device_id: a }))
      await assert.rejects(app.connectDevice({ kind: 'local', address, device_id: a }))
    }
    assert.deepEqual(sockets, []); assert.deepEqual(native, []); assert.deepEqual(nativeLocal, [])
    await assert.rejects(web.connectDevice({ kind: 'local', address: 'https://other.example', device_id: a }), /只能连接这个节点/)
    await assert.rejects(web.connectDevice({ kind: 'gateway', address: 'HTTPS://NODE.EXAMPLE:443/', device_id: a }), reached)
    await assert.rejects(web.connectDevice({ kind: 'local', address: 'https://page.example/', device_id: a }), reached)
    await assert.rejects(web.connectDevice({ kind: 'gateway', address: 'http://localhost:4790/', device_id: a }), reached)
    assert.deepEqual(sockets, ['wss://node.example/api/client', 'wss://page.example/api/local/ws', 'ws://localhost:4790/api/client'])
    await assert.rejects(app.connectDevice({ kind: 'gateway', address: 'https://node.example', device_id: a }), reached)
    assert.deepEqual(native, ['https://node.example'])
    await assert.rejects(app.connectDevice({ kind: 'local', address: 'https://node.example', device_id: a }), /在局域网里连不上这个节点/)
    assert.deepEqual(nativeLocal, ['https://node.example'], 'the inspected native-local carrier retains its explicit local-origin exemption')
    await assert.rejects(app.connectDevice({ kind: 'lan', addrs: ['999.999.999.999:80'], device_id: a }), /格式无效/)
    assert.deepEqual(dialed, [])
    await assert.rejects(app.connectDevice({ kind: 'lan', addrs: ['192.168.1.4:7001'], device_id: a }), /在局域网里连不上这个节点/)
    assert.deepEqual(dialed, [['192.168.1.4:7001']])
  } finally {
    for (const [name, descriptor] of [['location', previousLocation], ['WebSocket', previousSocket]] as const) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor)
      else Reflect.deleteProperty(globalThis, name)
    }
  }
})
