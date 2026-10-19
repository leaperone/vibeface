import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext, Script } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'
import { createHash } from 'node:crypto'
import initCrypto, { Identity, Handshake, verify } from '../../../generated/vibeface_secure.js'
import type { ConnectionEntry } from '../../lib/node.ts'
import { primaryLane, shouldAutoConnect } from './connection-lane.ts'
import { invitationLink } from '../../lib/enrollment-reference.ts'
import { readConnectionFile } from '../../lib/connection-file.ts'

test('connection links preserve the invitation in the fragment of the trusted web page', () => {
  const content = '{"url":"https://relay.example","peer_id":"pinned-node","relay":true,"capability":"配对凭证"}'
  const link = new URL(invitationLink(content, 'https://web.example/devices?old=secret#old'))
  assert.equal(link.origin, 'https://web.example')
  assert.equal(link.pathname, '/')
  assert.equal(link.search, '')
  const encoded = new URLSearchParams(link.hash.slice(1)).get('invite')
  assert.equal(typeof encoded, 'string')
  assert.ok(encoded)
  assert.equal(Buffer.from(encoded, 'base64url').toString('utf8'), '{"url":"https://relay.example","peer_id":"pinned-node","relay":true,"capability":"配对凭证"}')
  assert.equal(new URL(invitationLink('{}', 'http://localhost:4790')).origin, 'http://localhost:4790')
  for (const page of ['http://relay.example', 'https://user:password@web.example', 'file:///page']) assert.throws(() => invitationLink(content, page), /HTTPS|URL/)
})

test('connection files keep the original content and reject empty or oversized files', async () => {
  assert.equal(await readConnectionFile(new File(['{"capability":"original"}'], 'connection.json')), '{"capability":"original"}')
  await assert.rejects(readConnectionFile(new File([], 'empty.json')), /64 KiB/)
  await assert.rejects(readConnectionFile(new File([new Uint8Array(65537)], 'large.json')), /64 KiB/)
})

test('an invitation link opens the configured relay while preserving the pinned target', () => {
  const content = '{"url":"https://relay.example","peer_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","relay":true,"invite":{"capability":"original"}}'
  const link = new URL(invitationLink(content, 'https://relay.example'))
  assert.equal(link.origin, 'https://relay.example')
  const encoded = new URLSearchParams(link.hash.slice(1)).get('invite')
  assert.ok(encoded)
  const invitation: unknown = JSON.parse(Buffer.from(encoded, 'base64url').toString('utf8'))
  assert.deepEqual(invitation, { url: 'https://relay.example', peer_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', relay: true, invite: { capability: 'original' } })
  for (const page of ['http://127.0.0.1:4790', 'http://[::1]:4790']) assert.equal(new URL(invitationLink(content, page)).origin, page)
})

test('one saved entry connects itself, and several entries stay listed', () => {
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 1 }), true)
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 2 }), false)
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 0 }), false)
  assert.equal(shouldAutoConnect({ hasInvitation: true, savedCount: 1 }), false)
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 3, hasLocalSetup: false }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 0, hasLocalSetup: false }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1, hasLocalSetup: false }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2, hasLocalSetup: false }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1, hasLocalSetup: true }), 'local')
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 0, hasLocalSetup: true }), 'invite')
})

const target = 'a'.repeat(64), network = 'b'.repeat(64), subject = 'c'.repeat(64), publicKey = 'd'.repeat(64)
const entry: ConnectionEntry = { address: 'https://node.example', peer_id: target }
const localEntry: ConnectionEntry = { address: 'http://localhost:4790', peer_id: target }
const localCapability = 'f'.repeat(64)
const localInvitation = { url: localEntry.address, peer_id: target, capability: localCapability }
const localLaunch = { url: 'http://127.0.0.1:49781', peer_id: target, token: 'e'.repeat(64) }
const genesis = { version: 1, network_id: network, owner_public_key: publicKey }
const membership = { network_id: network, members: [], revoked: [], heads: [], epoch: 1 }
const canonical = (value: unknown): string => value && typeof value === 'object'
  ? Array.isArray(value) ? `[${value.map(canonical).join(',')}]` : `{${Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([key, item]) => `${JSON.stringify(key)}:${canonical(item)}`).join(',')}}`
  : JSON.stringify(value)
const deferred = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (reason: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settle = () => new Promise(setImmediate)
function nodeJoinFixture() {
  const expires_at_ms = Date.now() + 60000
  const payload = { invite_id: 'node-invite', network_id: network, inviter_subject_id: '9'.repeat(64), new_kind: 'node', manage: false, node_roles: ['relay'], epoch: 1, expires_at_ms }
  const invitation = JSON.stringify({ url: entry.address, peer_id: '8'.repeat(64), relay: false, invite: { capability: 'one-time-node-invite', invite: { payload, inviter_signature: 'signature' } } })
  const review = { version: 1, invite_id: payload.invite_id, network_id: network, inviter_subject_id: payload.inviter_subject_id, new_member: { subject_id: target, public_key: publicKey, kind: 'node', name: 'node', node_roles: ['relay'], manage: false }, proposal_hash: '7'.repeat(64), parents: [], epoch: 1, expires_at_ms }
  const receipt = { kind: 'review', request_id: 'node-pair', review, review_hash: createHash('sha256').update(canonical(review)).digest('hex') }
  return { invitation, receipt }
}

function deviceHarness({ pairing = false, bootstrap = false, saved = false, local = '', invitation = '', localSession = false, management = false, fragment = '', storage = new Map<string, string>() } = {}) {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const afterRender: (() => void)[] = [], timers = new Map<number, () => void>()
  const stored = storage, calls: string[] = [], navigations: string[] = []
  const requests: { method: string; params: Record<string, unknown> }[] = []
  const setupRequests: { path: string; options: RequestInit }[] = []
  const joinRequests: { action: string; params: unknown }[] = [], joinResponses = new Map<string, unknown>()
  const holds = new Map<string, ReturnType<typeof deferred>>()
  const sockets: { entry: ConnectionEntry; close(): void; drop(): void }[] = []
  const sessionEntry: ConnectionEntry = { address: 'https://node.example', peer_id: target, local: true }
  const localState: { enabled: boolean; setup?: unknown } = { enabled: true }
  const pageOrigin = local ? localEntry.address : entry.address
  const hash = invitation ? `#invite=${encodeURIComponent(invitation)}` : fragment
  const location = { hash, href: `${pageOrigin}/${hash}`, origin: pageOrigin, assign(url: string) { navigations.push(url) } }
  let cursor = 0, nextTimer = 0
  const react = {
    createContext: () => ({}), useContext: () => null, useCallback: (fn: unknown) => fn,
    useState(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = initial
      return [values[index], (next: unknown) => { values[index] = typeof next === 'function' ? next(values[index]) : next }]
    },
    useRef(initial: unknown) { const index = cursor++; if (!(index in values)) values[index] = { current: initial }; return values[index] },
    useEffect(run: () => (() => void) | void, dependencies: unknown[]) {
      const index = cursor++, previous = effects.get(index)
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) afterRender.push(() => {
        previous?.cleanup?.(); effects.set(index, { dependencies, cleanup: run() ?? undefined })
      })
    },
  }
  const review = { version: 1, invite_id: 'invite', network_id: network, inviter_subject_id: target, new_member: { subject_id: subject, public_key: publicKey, kind: 'client', name: 'browser', node_roles: [], manage: false }, proposal_hash: 'e'.repeat(64), parents: [], epoch: 1, expires_at_ms: Date.now() + 60000 }
  if (pairing) stored.set('vibeface.waiting-pair', JSON.stringify({ request_id: 'pair', network_id: network, entry, review, signing: { domain: 'identity-pair-review', payload: canonical(review) }, accepted: true }))
  if (saved) stored.set('vibeface.connections', JSON.stringify([entry]))
  const transport = {
    validTarget: (value: string) => value, validEndpoint: (value: string) => value,
    initClient: async () => ({ peerId: subject, publicKey }), readInvitation: JSON.parse, signClient: () => 'signature',
    async connectDevice(candidate: ConnectionEntry, options: { onClose(cause: Error): void; signal: AbortSignal }) {
      calls.push(`open:${candidate.peer_id}`)
      let closed = false
      const close = () => { if (closed) return; closed = true; options.onClose(new Error('connection lost')) }
      sockets.push({ entry: candidate, close, drop: close })
      options.signal.addEventListener('abort', close)
      if (holds.has('open')) await holds.get('open')?.promise
      return { peerId: candidate.peer_id, publicKey, close, async request(method: string, params: Record<string, unknown> = {}) {
        calls.push(method)
        requests.push({ method, params })
        if (holds.has(method)) return holds.get(method)?.promise
        if (method === 'info') return { peer_id: candidate.peer_id, public_key: publicKey, version: 'test', roles: ['relay'], initialized: !bootstrap && local !== 'fresh' }
        if (method === 'authenticate' && localSession) return { principal: { network_id: network } }
        if (method === 'local_access') { localState.enabled = false; return { enabled: false } }
        if (method === 'pair_status') return { status: 'granted' }
        if (method === 'prepare_genesis') {
          const genesis = { version: 1, network_id: network, owner_public_key: publicKey, signature: '', initial_members: [{ subject_id: target, public_key: publicKey, kind: 'node', name: 'This device', node_roles: ['relay'], manage: false }, { ...review.new_member, manage: true }] }
          return { state: { genesis, checkpoint: null, events: [] }, signing: { domain: 'identity-genesis', payload: canonical(genesis) } }
        }
        return {}
      } }
    },
  }
  let verifications = 0, released = 0
  const secure = {
    verifyAuthentication: () => { verifications++; return { auth: { local_auto_access: true }, principal: { source: localSession && !management ? 'local_session' : 'membership', network_id: network, subject_id: subject, epoch: 1, can_manage: management }, view: membership } },
    verifiedView: () => membership,
    openPackage: async () => ({ networkId: network, epoch: 1, payload: { genesis }, root: { publicKeyHex: () => publicKey, sign: () => 'signature' }, release() { released++ } }),
    Identity: { generate: () => ({ peerId: () => network, publicKeyHex: () => publicKey, secretHex: () => 'private-test-key', sign: () => 'signature', free() { released++ } }) },
    sealPackage: () => new Uint8Array([1, 2, 3]), downloadPackage() {}, verifyState: () => ({ free() {}, viewJson: () => JSON.stringify(membership) }),
  }
  const timer = (run: () => void) => { const id = ++nextTimer; timers.set(id, run); return id }
  const sandbox = createContext({
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    location, history: { replaceState(_state: unknown, _title: string, url: URL) { location.hash = url.hash } }, URL, URLSearchParams, AbortController, TextEncoder, Uint8Array, Error, crypto: globalThis.crypto,
    async fetch(path: string, options: RequestInit) {
      if (path === '/api/local/info') return localSession
        ? { ok: true, json: async () => ({ enabled: localState.enabled, has_session: localState.enabled, subject_id: subject, network_id: network, entry: sessionEntry }) }
        : { ok: false, status: 404 }
      if (path === '/api/local/exchange') {
        assert.equal(localSession, true)
        assert.equal(options.credentials, 'same-origin')
        assert.equal(options.redirect, 'error')
        assert.deepEqual(JSON.parse(String(options.body)), { token: 'f'.repeat(64), peer_id: target, subject_id: subject, public_key: publicKey })
        calls.push('local-exchange')
        return { ok: true, json: async () => ({ network_id: network, entry: sessionEntry }) }
      }
      if (path.startsWith('/api/local/join/')) {
        const action = path.slice('/api/local/join/'.length), key = `local_join:${action}`
        calls.push(key); joinRequests.push({ action, params: JSON.parse(String(options.body)) })
        const value = holds.has(key) ? await holds.get(key)?.promise : joinResponses.get(action) ?? { kind: 'idle' }
        return { status: 200, ok: true, json: async () => value }
      }
      calls.push('local_setup'); setupRequests.push({ path, options })
      const value = holds.has('local_setup') ? await holds.get('local_setup')?.promise : localState.setup ?? (local === 'fresh' ? { kind: 'uninitialized', invitation: localInvitation } : { kind: 'initialized', entry: localEntry })
      return { status: 200, ok: true, json: async () => value }
    },
    setTimeout: timer, clearTimeout: (id: number) => timers.delete(id), setInterval: timer, clearInterval: (id: number) => timers.delete(id),
  })
  const modules = new Map<string, { exports: Record<string, unknown> }>(), sourceRoot = fileURLToPath(new URL('../../', import.meta.url))
  function load(filename: string): Record<string, unknown> {
    const cached = modules.get(filename)
    if (cached) return cached.exports
    const module = { exports: {} }; modules.set(filename, module)
    const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
    const require = (specifier: string) => {
      if (specifier === 'react') return react
      if (specifier === 'react/jsx-runtime') return { jsx: (_type: unknown, props: unknown) => props }
      if (specifier.endsWith('/transport.js')) return transport
      if (specifier === '@/lib/secure') return secure
      const path = specifier.startsWith('@/') ? resolve(sourceRoot, specifier.slice(2)) : resolve(dirname(filename), specifier)
      return load(extname(path) ? path : `${path}.ts`)
    }
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(module.exports, require, module)
    return module.exports
  }
  const Provider = load(fileURLToPath(new URL('./context.tsx', import.meta.url))).DeviceProvider
  assert.equal(typeof Provider, 'function')
  type Device = ReturnType<typeof import('./context.tsx').useDevice>
  const render = (): Device => {
    cursor = 0
    assert.ok(typeof Provider === 'function')
    const props = Provider({ children: null })
    afterRender.splice(0).forEach(run => run())
    return props.value
  }
  return { render, calls, requests, setupRequests, joinRequests, joinResponses, holds, sockets, stored, timers, location, localState, navigations, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

test('fresh localhost discovers its one-time invitation without opening transport or saving the capability', async () => {
  const app = deviceHarness({ local: 'fresh', saved: true })
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup?.kind, 'uninitialized')
    assert.deepEqual(app.calls, ['local_setup', 'local_join:status'])
    assert.equal(app.setupRequests[0].path, '/api/local/setup')
    assert.equal(app.setupRequests[0].options.method, 'POST')
    assert.equal(app.setupRequests[0].options.body, '{}')
    assert.equal(app.setupRequests[0].options.cache, 'no-store')
    assert.equal(app.setupRequests[0].options.credentials, 'omit')
    assert.equal([...app.stored.values()].some(value => value.includes(localCapability)), false)
    await app.render().createLocalNetwork('browser', 'test-only-passphrase')
    assert.equal(app.render().review?.kind, 'bootstrap')
    assert.equal(app.calls.includes('bootstrap'), false)
    app.render().downloadBackup()
    await app.render().confirmBootstrap(true)
    assert.equal(app.render().ready?.entry.address, localEntry.address)
    assert.equal(app.render().localSetup?.kind, 'initialized')
    assert.equal(app.requests.find(request => request.method === 'bootstrap')?.params.capability, localCapability)
    assert.equal(app.calls.filter(call => call === 'bootstrap').length, 1)
  } finally { app.close() }
})

test('initialized localhost requires member authorization and cannot create a second network', async () => {
  const app = deviceHarness({ local: 'initialized' })
  const hold = deferred(); app.holds.set('authenticate', hold)
  try {
    app.render(); await settle()
    hold.reject(Object.assign(new Error('not a member'), { responded: true })); await settle()
    assert.equal(app.render().localSetup?.kind, 'initialized')
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.match(app.render().error, /尚未获得设备授权/)
    await assert.rejects(app.render().createLocalNetwork('browser', 'test-only-passphrase'), /已加入个人网络/)
    assert.equal(app.calls.includes('prepare_genesis'), false)
    assert.equal(app.calls.includes('bootstrap'), false)
  } finally { app.close() }
})

test('initialized localhost authenticates the signed manager on its original entry before offering local access', async () => {
  const app = deviceHarness({ local: 'initialized', management: true })
  app.localState.setup = { kind: 'initialized', entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'membership')
    assert.equal(app.render().ready?.principal.can_manage, true)
    assert.equal(app.render().ready?.entry.address, localEntry.address)
    assert.equal(app.render().ready?.entry.local, undefined)
    assert.equal(app.sockets[0]?.entry.address, localEntry.address)
    assert.equal(app.calls.includes('local-exchange'), false)
    assert.equal(app.navigations.length, 0)
    assert.equal(app.location.origin, localEntry.address)
  } finally { app.close() }
})

test('opening local access requires a click and fetches a fresh launcher before changing origin', async () => {
  const app = deviceHarness({ local: 'initialized' }), auth = deferred()
  app.localState.setup = { kind: 'initialized', entry: localEntry, local_launch: localLaunch }
  app.holds.set('authenticate', auth)
  try {
    app.render(); await settle()
    auth.reject(Object.assign(new Error('not a member'), { responded: true })); await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().localSetup?.kind, 'initialized')
    assert.equal(app.navigations.length, 0)
    assert.equal(app.calls.includes('local-exchange'), false)
    const fresh = { ...localLaunch, token: '1'.repeat(64) }
    app.localState.setup = { kind: 'initialized', entry: localEntry, local_launch: fresh }
    await app.render().openLocalAccess()
    assert.equal(app.setupRequests.length, 2)
    assert.equal(app.navigations.length, 1)
    const opened = new URL(app.navigations[0])
    assert.equal(opened.origin, fresh.url)
    assert.equal(new URLSearchParams(opened.hash.slice(1)).get('local'), fresh.token)
    assert.equal(new URLSearchParams(opened.hash.slice(1)).get('node'), target)
    assert.equal(app.joinRequests.length, 0)
    assert.equal([...app.stored.values()].some(value => value.includes(fresh.token)), false)
  } finally { app.close() }
})

test('disabling local automatic access before the click prevents launching its stale token', async () => {
  const app = deviceHarness({ local: 'initialized' }), auth = deferred()
  app.localState.setup = { kind: 'initialized', entry: localEntry, local_launch: localLaunch }
  app.holds.set('authenticate', auth)
  try {
    app.render(); await settle()
    auth.reject(Object.assign(new Error('not a member'), { responded: true })); await settle()
    app.localState.setup = { kind: 'initialized', entry: localEntry }
    await assert.rejects(app.render().openLocalAccess(), /自动访问已关闭/)
    assert.equal(app.navigations.length, 0)
    const setup = app.render().localSetup
    assert.equal(setup?.kind === 'initialized' && setup.local_launch, undefined)
  } finally { app.close() }
})

test('local launch metadata rejects an unrelated node, unsafe address, or malformed token', async () => {
  const invalid = [
    { ...localLaunch, peer_id: '0'.repeat(64) },
    { ...localLaunch, token: 'e'.repeat(63) },
    { ...localLaunch, token: 'E'.repeat(64) },
    ...['https://127.0.0.1:49781', 'http://localhost:49781', 'http://127.0.0.1', 'http://user:password@127.0.0.1:49781', 'http://127.0.0.1:49781/path', 'http://127.0.0.1:49781?token=unsafe', 'http://127.0.0.1:49781#unsafe'].map(url => ({ ...localLaunch, url })),
  ]
  for (const launch of invalid) {
    const app = deviceHarness({ local: 'initialized' })
    app.localState.setup = { kind: 'initialized', entry: localEntry, local_launch: launch }
    try {
      app.render(); await settle()
      assert.equal(app.render().localSetup, null)
      assert.match(app.render().error, /本机启动入口与此节点不一致/)
      assert.equal(app.sockets.length, 0)
      assert.equal(app.navigations.length, 0)
    } finally { app.close() }
  }
})

for (const kind of ['uninitialized', 'initialized']) test(`late ${kind} discovery cannot replace a manually chosen connection`, async () => {
  const app = deviceHarness({ local: 'fresh' })
  const hold = deferred(); app.holds.set('local_setup', hold)
  try {
    app.render(); await settle()
    app.render().disconnect()
    await app.render().connect(entry)
    hold.resolve(kind === 'uninitialized' ? { kind, invitation: localInvitation } : { kind, entry: localEntry })
    await settle()
    assert.equal(app.render().ready?.entry.address, entry.address)
    assert.equal(app.render().localSetup, null)
    assert.equal(app.calls.filter(call => call.startsWith('open:')).length, 1)
    assert.equal(app.calls.includes('bootstrap'), false)
  } finally { app.close() }
})

test('an invitation in the page URL takes precedence over local discovery', async () => {
  const invitation = JSON.stringify({ ...localInvitation, url: entry.address })
  const app = deviceHarness({ local: 'fresh', invitation })
  try {
    app.render(); await settle()
    assert.equal(app.render().initialInvitation, invitation)
    assert.equal(app.render().localSetup, null)
    assert.deepEqual(app.calls, [])
  } finally { app.close() }
})

test('a local setup response cannot redirect onboarding to another origin', async () => {
  const app = deviceHarness({ local: 'fresh' })
  const hold = deferred(); app.holds.set('local_setup', hold)
  try {
    app.render(); await settle()
    hold.resolve({ kind: 'uninitialized', invitation: { ...localInvitation, url: entry.address } }); await settle()
    assert.equal(app.render().localSetup, null)
    assert.match(app.render().error, /当前页面不一致/)
    assert.equal(app.sockets.length, 0)
  } finally { app.close() }
})

test('local setup cannot silently select the cookie channel instead of the ordinary member entry', async () => {
  const app = deviceHarness({ local: 'initialized' })
  app.localState.setup = { kind: 'initialized', entry: { ...localEntry, local: true }, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup, null)
    assert.match(app.render().error, /本机设置入口与当前页面不一致/)
    assert.equal(app.sockets.length, 0)
    assert.equal(app.navigations.length, 0)
  } finally { app.close() }
})

test('unmounting aborts pending local discovery without publishing setup state', async () => {
  const app = deviceHarness({ local: 'fresh' })
  const hold = deferred(); app.holds.set('local_setup', hold)
  app.render(); await settle()
  app.close()
  assert.equal(app.setupRequests[0].options.signal?.aborted, true)
  hold.resolve({ kind: 'uninitialized', invitation: localInvitation }); await settle()
  assert.deepEqual(app.calls, ['local_setup'])
  assert.equal(app.render().localSetup, null)
})

test('a node invitation uses the local Node endpoint and requires explicit acceptance of the displayed review', async () => {
  const app = deviceHarness({ local: 'fresh' }), { invitation, receipt } = nodeJoinFixture()
  try {
    app.render(); await settle()
    app.joinResponses.set('request', receipt)
    await app.render().pair(invitation, 'node')
    assert.equal(app.render().localJoin?.kind, 'review')
    assert.equal(app.joinRequests.filter(request => request.action === 'accept').length, 0)
    assert.equal(app.sockets.length, 0)
    assert.equal(app.render().ready, null)
    app.joinResponses.set('accept', { ...receipt, kind: 'pending' })
    await app.render().acceptLocalJoin()
    assert.equal(app.render().localJoin?.kind, 'pending')
    assert.deepEqual(app.joinRequests.find(request => request.action === 'accept')?.params, { request_id: 'node-pair', review_hash: receipt.review_hash })
    app.localState.setup = { kind: 'initialized', entry: localEntry, local_launch: localLaunch }
    app.joinResponses.set('status', { kind: 'joined', network_id: network, peer_id: target })
    await app.render().refreshLocalJoin()
    assert.equal(app.render().localJoin?.kind, 'joined')
    assert.equal(app.render().localSetup?.kind, 'initialized')
    const setup = app.render().localSetup
    assert.equal(setup?.kind === 'initialized' && setup.local_launch?.url, localLaunch.url)
    assert.equal(app.setupRequests.length, 3)
    assert.equal(app.navigations.length, 0)
    assert.equal(app.render().ready, null)
    assert.equal(app.calls.includes('authenticate'), false)
    assert.equal([...app.stored.values()].some(value => value.includes('one-time-node-invite')), false)
  } finally { app.close() }
})

test('refreshing a local Node restores its review without resending a request or accepting it', async () => {
  const app = deviceHarness({ local: 'fresh' }), { receipt } = nodeJoinFixture()
  app.joinResponses.set('status', receipt)
  try {
    app.render(); await settle(); await settle()
    assert.equal(app.render().localJoin?.kind, 'review')
    assert.deepEqual(app.joinRequests.map(request => request.action), ['status'])
    assert.equal(app.render().ready, null)
  } finally { app.close() }
})

test('Node joining verifies the displayed hash and refuses a mismatched local identity', async () => {
  for (const mismatch of ['hash', 'identity', 'invitation']) {
    const app = deviceHarness({ local: 'fresh' }), { invitation, receipt } = nodeJoinFixture()
    try {
      app.render(); await settle()
      if (mismatch === 'hash') app.joinResponses.set('request', { ...receipt, review_hash: '0'.repeat(64) })
      else {
        const review = { ...receipt.review, ...(mismatch === 'invitation' ? { network_id: '6'.repeat(64) } : { new_member: { ...receipt.review.new_member, subject_id: '5'.repeat(64) } }) }
        app.joinResponses.set('request', { ...receipt, review, review_hash: createHash('sha256').update(canonical(review)).digest('hex') })
      }
      await assert.rejects(app.render().pair(invitation, 'node'), /不一致/)
      assert.equal(app.render().localJoin?.kind, 'idle')
      assert.equal(app.joinRequests.some(request => request.action === 'accept'), false)
    } finally { app.close() }
  }
})

test('a lost Node acceptance response is reconciled through status without automatic resubmission', async () => {
  const app = deviceHarness({ local: 'fresh' }), { invitation, receipt } = nodeJoinFixture()
  try {
    app.render(); await settle()
    app.joinResponses.set('request', receipt)
    await app.render().pair(invitation, 'node')
    const hold = deferred(); app.holds.set('local_join:accept', hold)
    const accepting = app.render().acceptLocalJoin(), rejected = assert.rejects(accepting, /response lost/)
    await settle(); hold.reject(new Error('response lost')); await rejected
    assert.equal(app.joinRequests.filter(request => request.action === 'accept').length, 1)
    app.joinResponses.set('status', { ...receipt, kind: 'pending' })
    await app.render().refreshLocalJoin()
    assert.equal(app.render().localJoin?.kind, 'pending')
    assert.equal(app.joinRequests.filter(request => request.action === 'accept').length, 1)
  } finally { app.close() }
})

test('switching away while a Node join request is pending prevents a stale review', async () => {
  const app = deviceHarness({ local: 'fresh' }), { invitation, receipt } = nodeJoinFixture()
  try {
    app.render(); await settle()
    const hold = deferred(); app.holds.set('local_join:request', hold)
    const joining = app.render().pair(invitation, 'node'), rejected = assert.rejects(joining, /切换/)
    await settle(); assert.ok(app.calls.includes('local_join:request'))
    app.render().disconnect()
    hold.resolve(receipt); await rejected
    assert.equal(app.render().localJoin?.kind, 'idle')
    assert.equal(app.render().ready, null)
    assert.equal(app.sockets.length, 0)
  } finally { app.close() }
})

test('recovering onto a fresh localhost Node reuses local setup authority without a pasted invitation', async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await settle()
    const challenge = { network_id: network, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
    const challengeHold = deferred(), contextHold = deferred()
    app.holds.set('recovery_challenge', challengeHold); app.holds.set('recovery_context', contextHold)
    challengeHold.resolve({ challenge_id: 'challenge', challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
    const recovering = app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: localEntry, bootstrap: '' })
    const rejected = assert.rejects(recovering, /stop after authority check/)
    for (let tries = 0; tries < 100 && !app.calls.includes('recovery_context'); tries++) await new Promise(resolve => setTimeout(resolve, 1))
    assert.equal(app.requests.find(request => request.method === 'recovery_context')?.params.capability, localCapability)
    contextHold.reject(new Error('stop after authority check')); await rejected
    assert.equal(app.render().ready, null)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

test('localhost recovery can use a remote fresh Node invitation without sending local setup authority', async () => {
  const app = deviceHarness({ local: 'fresh' }), remote = { ...entry, peer_id: '6'.repeat(64) }, capability = '4'.repeat(64)
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup?.kind, 'uninitialized')
    const challenge = { network_id: network, owner_public_key: publicKey, subject_id: subject, target_peer_id: remote.peer_id, challenge_id: 'remote-challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
    const challengeHold = deferred(), contextHold = deferred()
    app.holds.set('recovery_challenge', challengeHold); app.holds.set('recovery_context', contextHold)
    challengeHold.resolve({ challenge_id: challenge.challenge_id, challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
    const recovering = app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: remote, bootstrap: JSON.stringify({ url: remote.address, peer_id: remote.peer_id, capability }) })
    const rejected = assert.rejects(recovering, /stop after remote authority check/)
    for (let tries = 0; tries < 100 && !app.calls.includes('recovery_context'); tries++) await new Promise(resolve => setTimeout(resolve, 1))
    const sent = app.requests.find(request => request.method === 'recovery_context')?.params.capability
    assert.equal(sent, capability)
    assert.notEqual(sent, localCapability)
    contextHold.reject(new Error('stop after remote authority check')); await rejected
    assert.equal(app.render().ready, null)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

test('a local browser reuses its cookie-bound identity and can disable its own automatic access', async () => {
  const app = deviceHarness({ localSession: true })
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    assert.equal(app.render().ready?.principal.can_manage, false)
    assert.equal(app.render().localAutoAccess, true)
    assert.equal(app.calls.filter(method => method === 'pair_request').length, 0)
    assert.equal(app.sockets[0]?.entry.address, 'https://node.example')
    assert.equal(app.sockets[0]?.entry.peer_id, target)
    assert.equal(app.sockets[0]?.entry.local, true)
    assert.equal(Object.hasOwn(app.sockets[0]?.entry ?? {}, 'relay'), false)
    await app.render().setLocalAutoAccess(false)
    assert.equal(app.render().ready, null)
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
    assert.equal(app.calls.filter(method => method === 'local_access').length, 1)
  } finally { app.close() }
})

test('a signed management browser keeps its permissions through the local cookie channel, reconnect, and reload', async () => {
  const storage = new Map<string, string>()
  for (let reload = 0; reload < 2; reload++) {
    const app = deviceHarness({ localSession: true, management: true, storage })
    try {
      app.render(); await settle()
      assert.equal(app.sockets[0]?.entry.local, true)
      assert.equal(app.render().ready?.principal.source, 'membership')
      assert.equal(app.render().ready?.principal.can_manage, true)
      assert.equal(app.render().ready?.entry.local, false)
      assert.equal(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? 'null').entry.local, false)
      app.sockets[0]?.drop()
      const [id, retry] = [...app.timers.entries()][0]
      app.timers.delete(id); retry(); await settle()
      assert.equal(app.sockets[1]?.entry.local, undefined)
      assert.equal(app.render().ready?.principal.source, 'membership')
      assert.equal(app.render().ready?.principal.can_manage, true)
      assert.equal(app.verifications(), 2)
    } finally { app.close() }
  }
})

test('the launcher fragment is cleared before its token is exchanged for the browser key', async () => {
  const app = deviceHarness({ localSession: true, fragment: `#local=${'f'.repeat(64)}&node=${target}` })
  try {
    app.render()
    assert.equal(app.location.hash, '')
    await settle()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    assert.equal(app.calls.filter(method => method === 'local-exchange').length, 1)
    assert.equal([...app.stored.values()].some(value => value.includes('f'.repeat(64))), false)
  } finally { app.close() }
})

test('revoking local access in another tab stops reconnecting the revoked cookie', async () => {
  const app = deviceHarness({ localSession: true })
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    app.localState.enabled = false
    app.sockets[0]?.drop()
    const retry = app.timers.values().next().value
    assert.equal(typeof retry, 'function')
    retry?.(); await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.equal(app.calls.filter(method => method.startsWith('open:')).length, 1)
  } finally { app.close() }
})

for (const responded of [true, false]) test(`a ${responded ? 'rejected' : 'disconnected'} local access change preserves its real outcome`, async () => {
  const app = deviceHarness({ localSession: true })
  try {
    app.render(); await settle()
    const pending = deferred(); app.holds.set('local_access', pending)
    const changing = app.render().setLocalAutoAccess(false)
    const rejected = assert.rejects(changing, /configuration unavailable/)
    pending.reject(Object.assign(new Error('configuration unavailable'), { responded }))
    await rejected
    if (responded) {
      assert.equal(app.render().ready?.principal.source, 'local_session')
      assert.equal(app.render().localAutoAccess, true)
    } else {
      assert.equal(app.render().ready, null)
      assert.match(app.render().error, /结果尚未确认/)
    }
  } finally { app.close() }
})

for (const stage of ['authenticate', 'subscribe']) test(`cancelPair during ${stage} cannot restore the cancelled connection`, async () => {
  const app = deviceHarness({ pairing: true })
  try {
    app.render(); await settle(); app.render()
    const hold = deferred(); app.holds.set(stage, hold)
    const checking = app.render().checkPair()
    const rejected = assert.rejects(checking, /切换|取消/)
    await settle()
    assert.ok(app.calls.includes(stage))
    app.render().cancelPair()
    hold.resolve({})
    await rejected; await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().waiting, null)
    assert.equal(app.timers.size, 0)
    assert.equal(app.stored.get('vibeface.last-authenticated') ?? 'null', 'null')
    assert.equal(app.calls.filter(call => call.startsWith('open:')).length, 1)
  } finally { app.close() }
})

for (const stage of ['bootstrap', 'subscribe']) test(`cancelSensitive during ${stage} cannot publish ready or restart transport`, async () => {
  const app = deviceHarness({ bootstrap: true })
  try {
    app.render(); await settle()
    await app.render().pair(JSON.stringify({ url: entry.address, peer_id: target, relay: true, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
    app.render().downloadBackup()
    const hold = deferred(); app.holds.set(stage, hold)
    const confirming = app.render().confirmBootstrap(true)
    const rejected = assert.rejects(confirming, /切换|取消/)
    await settle(); assert.ok(app.calls.includes(stage))
    app.render().cancelSensitive()
    hold.resolve({})
    await rejected; await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().review, null)
    assert.equal(app.timers.size, 0)
    assert.equal(app.stored.get('vibeface.last-authenticated') ?? 'null', 'null')
    assert.equal(app.calls.filter(call => call === 'bootstrap').length, 1)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

for (const flow of ['pair', 'sensitive']) test(`cancelling ${flow} after subscribe resolves still prevents stale authentication publication`, async () => {
  const app = deviceHarness({ pairing: flow === 'pair', bootstrap: flow === 'sensitive' })
  try {
    app.render(); await settle(); app.render()
    if (flow === 'sensitive') {
      await app.render().pair(JSON.stringify({ url: entry.address, peer_id: target, relay: true, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
      app.render().downloadBackup()
    }
    const hold = deferred(); app.holds.set('subscribe', hold)
    const checking = flow === 'pair' ? app.render().checkPair() : app.render().confirmBootstrap(true)
    const rejected = assert.rejects(checking, /切换|取消/)
    await settle(); assert.ok(app.calls.includes('subscribe'))
    hold.resolve({})
    queueMicrotask(() => queueMicrotask(() => queueMicrotask(() => {
      if (flow === 'pair') app.render().cancelPair(); else app.render().cancelSensitive()
    })))
    await rejected; await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'disconnected')
    assert.equal(app.stored.get('vibeface.last-authenticated') ?? 'null', 'null')
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

for (const flow of ['directory', 'recovery']) test(`cancelling after ${flow} response resolves prevents stale state and material publication`, async () => {
  const app = deviceHarness({ saved: flow === 'directory' })
  try {
    app.render(); await settle(); app.render()
    const hold = deferred(); app.holds.set(flow === 'directory' ? 'directory' : 'recovery_context', hold)
    let work: Promise<void>
    if (flow === 'directory') work = app.render().refreshDirectory()
    else {
      const challenge = { network_id: network, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
      const challengeHold = deferred(); app.holds.set('recovery_challenge', challengeHold)
      challengeHold.resolve({ challenge_id: 'challenge', challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
      work = app.render().beginRecovery({ file: new File(['test-fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', bootstrap: '', entry })
    }
    const rejected = assert.rejects(work, /切换|取消/)
    for (let tries = 0; tries < 100 && !app.calls.includes(flow === 'directory' ? 'directory' : 'recovery_context'); tries++) await new Promise(resolve => setTimeout(resolve, 1))
    assert.ok(app.calls.includes(flow === 'directory' ? 'directory' : 'recovery_context'))
    hold.resolve({ state: { genesis }, view: membership })
    queueMicrotask(() => queueMicrotask(() => queueMicrotask(() => app.render().disconnect())))
    await rejected; await settle()
    assert.equal(app.render().phase, 'disconnected')
    assert.equal(app.render().directory, null)
    assert.equal(app.render().review, null)
    assert.equal(app.released(), flow === 'recovery' ? 1 : 0)
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('granted pairing restores only verified authentication and subscription, and disconnect cancels retry', async () => {
  const app = deviceHarness({ pairing: true })
  try {
    app.render(); await settle(); app.render()
    await app.render().checkPair()
    assert.equal(app.render().ready?.entry.peer_id, target)
    assert.equal(app.verifications(), 1)
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'pair_status', 'authenticate', 'subscribe'])
    app.sockets[0].drop(); app.render()
    assert.equal(app.timers.size, 1)
    app.render().disconnect()
    assert.equal(app.timers.size, 0)
    assert.equal(app.render().ready, null)
  } finally { app.close() }
})

test('unexpected closure reconnects the same target with fresh authentication and subscription only', async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    const previous = app.render().ready?.generation
    app.sockets[0].drop()
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    assert.equal(app.render().ready?.entry.peer_id, target)
    assert.notEqual(app.render().ready?.generation, previous)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.verifications(), 2)
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'authenticate', 'subscribe', `open:${target}`, 'info', 'authenticate', 'subscribe'])
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('revoked authorization stops reconnection and releases the rejected session', async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    app.sockets[0].drop()
    const hold = deferred(); app.holds.set('authenticate', hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    hold.reject(Object.assign(new Error('authorization revoked'), { responded: true })); await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.match(app.render().error, /无法恢复原网络授权/)
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
    assert.equal(app.timers.size, 0)
    app.sockets[1].drop(); assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('successful bootstrap releases local material without disconnecting the authenticated session', async () => {
  const app = deviceHarness({ bootstrap: true })
  try {
    app.render(); await settle()
    await app.render().pair(JSON.stringify({ url: entry.address, peer_id: target, relay: true, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
    app.render().downloadBackup()
    await app.render().confirmBootstrap(true)
    assert.equal(app.render().ready?.entry.peer_id, target)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.render().review, null)
    assert.equal(app.calls.filter(call => call === 'bootstrap').length, 1)
    assert.equal(app.verifications(), 1)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

for (const original of ['auto-authenticate', 'pair-poll', 'waiting-info']) test(`late ${original} failure cannot overwrite another connection`, async () => {
  const app = deviceHarness({ saved: original === 'auto-authenticate', pairing: original !== 'auto-authenticate' })
  try {
    const hold = deferred(), method = original === 'auto-authenticate' ? 'authenticate' : original === 'pair-poll' ? 'pair_status' : 'info'
    app.holds.set(method, hold)
    app.render(); await settle(); app.render()
    if (original === 'pair-poll') { assert.equal(app.timers.size, 1); app.timers.values().next().value?.(); await settle() }
    assert.ok(app.calls.includes(method))
    app.render().disconnect()
    app.holds.delete(method)
    const replacement = { ...entry, peer_id: 'f'.repeat(64) }
    await app.render().connect(replacement)
    hold.reject(Object.assign(new Error('old permission rejected'), { responded: true }))
    await settle()
    assert.equal(app.render().ready?.entry.peer_id, replacement.peer_id)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.render().phase, 'ready')
    assert.equal(app.render().error, '')
  } finally { app.close() }
})

for (const stage of ['open', 'authenticate']) test(`switching devices during reconnect ${stage} invalidates the old retry`, async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().ready?.entry.peer_id, target)
    app.sockets[0].drop(); app.sockets[0].drop()
    assert.equal(app.timers.size, 1)
    const hold = deferred(); app.holds.set(stage, hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry()
    await settle()
    assert.equal(app.calls.filter(call => call === `open:${target}`).length, 2)
    app.render().disconnect()
    app.holds.delete(stage)
    const replacement = { ...entry, peer_id: 'f'.repeat(64) }
    await app.render().connect(replacement)
    hold.resolve({})
    await settle()
    assert.equal(app.render().ready?.entry.peer_id, replacement.peer_id)
    assert.equal(app.render().phase, 'ready')
    assert.equal(app.render().error, '')
    assert.equal(app.timers.size, 0)
    assert.equal(app.calls.filter(call => call === `open:${target}`).length, 2)
    assert.equal(app.calls.filter(call => ['bootstrap', 'pair_request', 'rpc'].includes(call)).length, 0)
  } finally { app.close() }
})


async function routeHarness({ invalid = '', hold = false, secure = false, legacy = false, local = false, localFail = false } = {}) {
  await initCrypto({ module_or_path: readFileSync(new URL('../../../generated/vibeface_secure_bg.wasm', import.meta.url)) })
  const client = Identity.generate(), targetKey = Identity.generate()
  const targetUrl = `${secure ? 'wss' : 'ws'}://${local ? 'entry.example/api/local/ws' : legacy ? 'entry.example/api/network/ws' : 'target.example/custom/network'}`
  const now = Math.floor(Date.now() / 1000)
  const record = { peer_id: targetKey.peerId(), public_key: targetKey.publicKeyHex(), direct_urls: ['ws://offline.example/path'], relay_urls: [targetUrl], issued_at: now, expires_at: now + 120, signature: '' }
  record.signature = targetKey.sign('reachability', new TextEncoder().encode(JSON.stringify(record)))
  if (invalid === 'signature') record.relay_urls = ['ws://attacker.example/path']
  if (invalid === 'expired') record.expires_at = now - 1
  if (invalid === 'target') record.peer_id = 'f'.repeat(64)
  const sockets = new Set<RecordSocket>(), routes: string[] = [], controls: Record<string, unknown>[] = []
  let maximum = 0, data = 0, closedEvents = 0
  class RecordSocket extends EventTarget {
    static OPEN = 1
    readyState = 0
    binaryType = 'arraybuffer'
    dead = false
    handshake: Handshake | undefined
    channel: ReturnType<Handshake['finish']> | undefined
    isData = false
    readonly url: string
    constructor(url: string) {
      super(); this.url = url; sockets.add(this)
      queueMicrotask(() => {
        this.readyState = RecordSocket.OPEN
        if (local) {
          routes.push(this.url); this.isData = true; data++; maximum = Math.max(maximum, data)
          if (localFail) { this.close(); return }
          this.handshake = new Handshake(false, targetKey.secretHex(), undefined, new TextEncoder().encode('vibeface:client:v1'))
          this.dispatchEvent(new Event('open'))
        } else this.emit(JSON.stringify({ type: 'welcome', nonce: 'a'.repeat(64) }))
      })
    }
    emit(value: unknown) { if (!this.dead) this.dispatchEvent(new MessageEvent('message', { data: value })) }
    encrypted(value: Uint8Array) { const copy = new Uint8Array(value); queueMicrotask(() => this.emit(copy.buffer)) }
    send(value: string | Uint8Array) {
      if (typeof value === 'string') {
        const message = JSON.parse(value); controls.push(message)
        if (message.type === 'lookup') { if (!hold) queueMicrotask(() => this.emit(JSON.stringify({ type: 'record', record: legacy ? null : record }))); return }
        routes.push(this.url); this.isData = true; data++; maximum = Math.max(maximum, data)
        if (hold) return
        if (this.url !== targetUrl || legacy && message.type === 'dial') { queueMicrotask(() => this.close()); return }
        this.handshake = new Handshake(false, targetKey.secretHex(), undefined, new TextEncoder().encode('vibeface:client:v1'))
        queueMicrotask(() => this.emit(JSON.stringify({ type: 'ready' })))
        return
      }
      if (this.handshake) {
        this.handshake.read(value)
        if (this.handshake.isFinished()) { this.channel = this.handshake.finish(); this.handshake.free(); this.handshake = undefined }
        else this.encrypted(this.handshake.write(new Uint8Array()))
      } else if (this.channel) {
        const request = this.channel.openRecord(value)
        if (request) for (const frame of this.channel.sealMessage(new TextEncoder().encode(JSON.stringify({ id: JSON.parse(new TextDecoder().decode(request)).id, result: { peer_id: targetKey.peerId() } })))) this.encrypted(frame)
      }
    }
    close() {
      if (this.dead) return
      this.dead = true; sockets.delete(this); if (this.isData) data--
      this.handshake?.free(); this.channel?.free(); this.dispatchEvent(new Event('close'))
    }
  }
  const source = readFileSync(new URL('../../../transport.js', import.meta.url), 'utf8').replace(/^import .*\n/, '').replace(/^export /gm, '')
  const runtime = new Script(`${source}\nidentity = testIdentity; ({ connectDevice });`).runInNewContext({
    init: initCrypto, Identity, Handshake, verify, testIdentity: client, TextEncoder, TextDecoder, Uint8Array, ArrayBuffer,
    WebSocket: RecordSocket, URL, crypto: globalThis.crypto, location: { protocol: secure ? 'https:' : 'http:', origin: `${secure ? 'https' : 'http'}://entry.example` }, AbortController, setTimeout, clearTimeout,
  })
  return {
    entry: { address: `${secure ? 'https' : 'http'}://entry.example`, peer_id: targetKey.peerId(), ...(local ? { local: true } : {}) }, controls, routes,
    connect(signal?: AbortSignal) { return runtime.connectDevice(this.entry, { signal, onClose() { closedEvents++ } }) },
    maximum: () => maximum, closedEvents: () => closedEvents, open: () => sockets.size,
    close() { for (const socket of [...sockets]) socket.close(); client.free(); targetKey.free() },
  }
}

test('a local session starts pinned Noise on its own cookie endpoint without network discovery or fallback', async () => {
  for (const localFail of [false, true]) {
    const app = await routeHarness({ local: true, localFail })
    try {
      if (localFail) await assert.rejects(app.connect())
      else {
        const channel = await app.connect()
        assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
        channel.close(); await settle()
      }
      assert.deepEqual(app.controls, [])
      assert.deepEqual(app.routes, ['ws://entry.example/api/local/ws'])
      assert.equal(app.open(), 0)
      app.entry.address = 'http://other.example'
      await assert.rejects(app.connect(), /本机/)
      assert.equal(app.routes.length, 1)
    } finally { app.close() }
  }
})

test('automatic transport verifies a signed other-entry path, authenticates the target, and closes only the selected session', async () => {
  const app = await routeHarness()
  try {
    const channel = await app.connect()
    assert.equal(channel.peerId, app.entry.peer_id)
    assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
    assert.equal(app.controls.find(message => message.type === 'dial')?.target, app.entry.peer_id)
    assert.equal(app.routes.includes('ws://target.example/custom/network'), true)
    assert.equal(app.routes.includes('ws://offline.example/path'), true)
    assert.ok(app.maximum() <= 2)
    assert.equal(app.closedEvents(), 0)
    channel.close(); await settle()
    assert.equal(app.closedEvents(), 1)
    assert.equal(app.open(), 0)
  } finally { app.close() }
})

for (const invalid of ['signature', 'expired', 'target']) test(`automatic transport rejects a ${invalid} directory record without opening its candidates`, async () => {
  const app = await routeHarness({ invalid })
  try {
    await assert.rejects(app.connect())
    assert.deepEqual(app.routes, ['ws://entry.example/api/network/ws', 'ws://entry.example/api/network/ws'])
    assert.deepEqual(app.controls.filter(message => ['dial', 'direct'].includes(String(message.type))).map(message => message.type), ['dial', 'direct'])
    assert.equal(app.closedEvents(), 0)
    assert.equal(app.open(), 0)
  } finally { app.close() }
})

test('an older local entry falls back to direct only after target dial fails and still pins its identity', async () => {
  const app = await routeHarness({ legacy: true })
  try {
    const channel = await app.connect()
    assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
    assert.deepEqual(app.controls.filter(message => ['dial', 'direct'].includes(String(message.type))).map(message => message.type), ['dial', 'direct'])
    assert.deepEqual(app.routes, ['ws://entry.example/api/network/ws', 'ws://entry.example/api/network/ws'])
    assert.equal(app.maximum(), 1)
    channel.close(); await settle()
    assert.equal(app.open(), 0)
  } finally { app.close() }
})

test('cancelling automatic transport releases the active route and directory query', async () => {
  const app = await routeHarness({ hold: true }), controller = new AbortController()
  try {
    const connecting = app.connect(controller.signal)
    const rejected = assert.rejects(connecting, /取消/)
    await settle(); controller.abort(); await rejected; await settle()
    assert.equal(app.open(), 0)
    assert.equal(app.closedEvents(), 0)
  } finally { app.close() }
})

test('saved current-format route preferences collapse into one target without losing its identity', async () => {
  const app = deviceHarness()
  try {
    app.stored.set('vibeface.connections', JSON.stringify([{ ...entry, relay: false }, { ...entry, relay: true }]))
    app.render(); await settle()
    const entries = app.render().entries
    assert.equal(entries.length, 1)
    assert.equal(entries[0].peer_id, entry.peer_id)
    assert.equal(Object.hasOwn(entries[0], 'relay'), false)
  } finally { app.close() }
})


test('a secure page skips an unusable insecure candidate while retaining its signed secure relay', async () => {
  const app = await routeHarness({ secure: true })
  try {
    const channel = await app.connect()
    assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
    assert.equal(app.routes.includes('ws://offline.example/path'), false)
    assert.equal(app.routes.includes('wss://target.example/custom/network'), true)
    channel.close(); await settle()
    assert.equal(app.open(), 0)
  } finally { app.close() }
})
