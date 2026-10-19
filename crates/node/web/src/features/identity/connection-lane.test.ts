import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext, Script } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'
import { createHash, generateKeyPairSync, createPublicKey, sign as signBytes, verify as verifyBytes } from 'node:crypto'
import type { ClusterTarget, ConnectionEntry, MembershipView, Member } from '../../lib/node.ts'
import initCrypto, { Identity, Handshake, verify } from '../../../generated/vibeface_secure.js'
import type { ClusterPath } from '../../../transport.js'
import { primaryLane, shouldAutoConnect } from './connection-lane.ts'
import { invitationLink } from '../../lib/enrollment-reference.ts'
import { readConnectionFile } from '../../lib/connection-file.ts'

test('connection links preserve the invitation in the fragment of the trusted web page', () => {
  const content = '{"url":"https://relay.example","peer_id":"pinned-node","capability":"配对凭证"}'
  const link = new URL(invitationLink(content, 'https://web.example/devices?old=secret#old'))
  assert.equal(link.origin, 'https://web.example')
  assert.equal(link.pathname, '/')
  assert.equal(link.search, '')
  const encoded = new URLSearchParams(link.hash.slice(1)).get('invite')
  assert.equal(typeof encoded, 'string')
  assert.ok(encoded)
  assert.equal(Buffer.from(encoded, 'base64url').toString('utf8'), '{"url":"https://relay.example","peer_id":"pinned-node","capability":"配对凭证"}')
  assert.equal(new URL(invitationLink('{}', 'http://localhost:4790')).origin, 'http://localhost:4790')
  for (const page of ['http://relay.example', 'https://user:password@web.example', 'file:///page']) assert.throws(() => invitationLink(content, page), /HTTPS|URL/)
})

test('invitation transport endpoints preserve custom WSS paths while links use the HTTPS page', () => {
  const endpoint = 'wss://relay.example/custom/ws'
  const content = JSON.stringify({ url: endpoint, peer_id: target, capability: 'bootstrap' })
  const link = new URL(invitationLink(content, endpoint))
  assert.equal(link.origin, 'https://relay.example')
  assert.equal(link.pathname, '/')
  const encoded = new URLSearchParams(link.hash.slice(1)).get('invite') ?? ''
  const restored = JSON.parse(Buffer.from(encoded, 'base64url').toString('utf8'))
  assert.equal(restored.url, endpoint)
  assert.equal(restored.peer_id, target)
})

test('connection files keep the original content and reject empty or oversized files', async () => {
  assert.equal(await readConnectionFile(new File(['{"capability":"original"}'], 'connection.json')), '{"capability":"original"}')
  await assert.rejects(readConnectionFile(new File([], 'empty.json')), /64 KiB/)
  await assert.rejects(readConnectionFile(new File([new Uint8Array(65537)], 'large.json')), /64 KiB/)
})

test('an invitation link opens the configured relay while preserving the pinned target', () => {
  const content = '{"url":"https://relay.example","peer_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","invite":{"capability":"original"}}'
  const link = new URL(invitationLink(content, 'https://relay.example'))
  assert.equal(link.origin, 'https://relay.example')
  const encoded = new URLSearchParams(link.hash.slice(1)).get('invite')
  assert.ok(encoded)
  const invitation: unknown = JSON.parse(Buffer.from(encoded, 'base64url').toString('utf8'))
  assert.deepEqual(invitation, { url: 'https://relay.example', peer_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', invite: { capability: 'original' } })
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

const target = 'a'.repeat(64), cluster = 'b'.repeat(64), subject = 'c'.repeat(64), publicKey = 'd'.repeat(64)
const entry: ConnectionEntry = { address: 'https://node.example', peer_id: target }
const localEntry: ConnectionEntry = { address: 'http://localhost:4790', peer_id: target }
const localCapability = 'f'.repeat(64)
const localInvitation = { url: localEntry.address, peer_id: target, capability: localCapability }
const localLaunch = { url: 'http://127.0.0.1:49781', peer_id: target, token: 'e'.repeat(64) }
const genesis = { version: 2, cluster_id: cluster, owner_public_key: publicKey }
const membership: MembershipView = { cluster_id: cluster, members: [], revoked: [], heads: [], epoch: 1 }
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
  const payload = { version: 2, invite_id: 'node-invite', cluster_id: cluster, inviter_subject_id: '9'.repeat(64), new_kind: 'node', manage: false, node_roles: ['relay'], epoch: 1, expires_at_ms }
  const invitation = JSON.stringify({ url: entry.address, peer_id: '8'.repeat(64), invite: { capability: 'one-time-node-invite', invite: { payload, inviter_signature: 'signature' } } })
  const review = { version: 2, invite_id: payload.invite_id, cluster_id: cluster, inviter_subject_id: payload.inviter_subject_id, new_member: { subject_id: target, public_key: publicKey, kind: 'node', name: 'node', node_roles: ['relay'], manage: false }, proposal_hash: '7'.repeat(64), parents: [], epoch: 1, expires_at_ms }
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
  const sockets: { entry: ConnectionEntry; path?: ClusterPath; close(): void; drop(): void }[] = []
  const localState: { enabled: boolean; setup?: unknown } = { enabled: true }
  const pageOrigin = local ? localEntry.address : entry.address
  const sessionEntry: ConnectionEntry = { address: pageOrigin, peer_id: target, local: true }
  const clock = { now: Date.now() }
  const hash = invitation ? `#invite=${encodeURIComponent(invitation)}` : fragment
  const location = { hash, href: `${pageOrigin}/${hash}`, origin: pageOrigin, assign(url: string) { navigations.push(url) } }
  const discovery: { records: { peerId: string; publicKey: string; expiresAt: number; paths: ClusterPath[] }[]; signals: AbortSignal[]; rejected: Set<string> } = { records: [], signals: [], rejected: new Set() }
  const signed: { view: MembershipView; mergeError?: Error; rejectNextAuthentication?: boolean } = { view: membership }
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
  const review = { version: 2, invite_id: 'invite', cluster_id: cluster, inviter_subject_id: target, new_member: { subject_id: subject, public_key: publicKey, kind: 'client', name: 'browser', node_roles: [], manage: false }, proposal_hash: 'e'.repeat(64), parents: [], epoch: 1, expires_at_ms: Date.now() + 60000 }
  if (pairing) stored.set('vibeface.waiting-pair', JSON.stringify({ request_id: 'pair', cluster_id: cluster, entry, review, signing: { domain: 'identity-pair-review', payload: canonical(review) }, accepted: true }))
  if (saved) stored.set('vibeface.connections', JSON.stringify([entry]))
  const transport = {
    async discoverCluster(_target: unknown, options: { signal: AbortSignal }) {
      calls.push('discover'); discovery.signals.push(options.signal)
      if (holds.has('discover')) await holds.get('discover')?.promise
      return discovery.records
    },
    validTarget: (value: string) => value, validEndpoint: (value: string) => value,
    initClient: async () => ({ peerId: subject, publicKey }), readInvitation: JSON.parse, signClient: () => 'signature',
    connectClusterPath(candidate: ClusterPath & { peer_id: string }, options: { onClose(cause: Error): void; signal: AbortSignal }) {
      const connection = transport.connectDevice({ address: candidate.address, peer_id: candidate.peer_id }, options)
      sockets[sockets.length - 1].path = { address: candidate.address, mode: candidate.mode }
      return connection
    },
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
        if (holds.has(method)) { const held = await holds.get(method)?.promise; if (method !== 'authenticate') return held }
        if (method === 'info') return { peer_id: candidate.peer_id, public_key: publicKey, version: 'test', roles: ['gateway', 'replica'], initialized: !bootstrap && local !== 'fresh' }
        if (method === 'authenticate') { if (discovery.rejected.has(candidate.peer_id)) throw Object.assign(new Error('entry has not synchronized authorization'), { responded: true }); return { principal: { cluster_id: cluster }, state: { genesis } } }
        if (method === 'directory') return { state: { genesis }, view: signed.view }
        if (method === 'local_access') { localState.enabled = false; return { enabled: false } }
        if (method === 'pair_status') return { status: 'granted' }
        if (method === 'prepare_genesis') {
          const genesis = { version: 2, cluster_id: cluster, owner_public_key: publicKey, signature: '', initial_members: [{ subject_id: target, public_key: publicKey, kind: 'node', name: 'This device', node_roles: ['gateway', 'replica'], manage: false }, { ...review.new_member, manage: true }] }
          return { state: { genesis, checkpoint: null, events: [] }, signing: { domain: 'identity-genesis', payload: canonical(genesis) } }
        }
        return {}
      } }
    },
  }
  let verifications = 0, released = 0
  const secure = {
    mergeMembership: (state: unknown) => { if (signed.mergeError) throw signed.mergeError; return { state: state ?? { genesis }, view: signed.view } },
    authorizeMembership: () => { if (signed.view.revoked.includes(subject)) throw new secure.MembershipRevokedError('signed revocation'); return { manage: management } },
    MembershipRevokedError: class extends Error {},
    verifyAuthentication: (_auth: unknown, _own: unknown, trust: { kind: string } | null) => { verifications++; if (signed.view.revoked.includes(subject)) throw new secure.MembershipRevokedError('signed revocation'); if (signed.rejectNextAuthentication) { signed.rejectNextAuthentication = false; throw new Error('entry no longer authorized') }; return { auth: { local_auto_access: true }, principal: { source: trust?.kind === 'local' && !management ? 'local_session' : 'membership', cluster_id: cluster, subject_id: subject, epoch: 1, can_manage: management }, view: signed.view } },
    verifiedView: () => signed.view,
    openPackage: async () => ({ clusterId: cluster, epoch: 1, payload: { genesis }, root: { publicKeyHex: () => publicKey, sign: () => 'signature' }, release() { released++ } }),
    Identity: { generate: () => ({ peerId: () => cluster, publicKeyHex: () => publicKey, secretHex: () => 'private-test-key', sign: () => 'signature', free() { released++ } }) },
    sealPackage: () => new Uint8Array([1, 2, 3]), downloadPackage() {},
    verifyState: () => ({ free() {}, viewJson: () => JSON.stringify(membership), createRecoveryCheckpoint: () => canonical({ epoch: 2 }) }),
    verifyRecoveryResult: (auth: unknown) => ({ auth, epoch: 2 }),
  }
  const timer = (run: () => void) => { const id = ++nextTimer; timers.set(id, run); return id }
  const sandbox = createContext({
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    location, history: { replaceState(_state: unknown, _title: string, url: URL) { location.hash = url.hash } }, URL, URLSearchParams, AbortController, TextEncoder, Uint8Array, Error, crypto: globalThis.crypto, Date: class extends Date { static now() { return clock.now } },
    async fetch(path: string, options: RequestInit) {
      if (path === '/api/local/info') return localSession
        ? { ok: true, json: async () => ({ enabled: localState.enabled, has_session: localState.enabled, subject_id: subject, cluster_id: cluster, entry: sessionEntry }) }
        : { ok: false, status: 404 }
      if (path === '/api/local/exchange') {
        assert.equal(localSession, true)
        assert.equal(options.credentials, 'same-origin')
        assert.equal(options.redirect, 'error')
        assert.deepEqual(JSON.parse(String(options.body)), { token: 'f'.repeat(64), peer_id: target, subject_id: subject, public_key: publicKey })
        calls.push('local-exchange')
        return { ok: true, json: async () => ({ cluster_id: cluster, entry: sessionEntry }) }
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
  return { render, calls, requests, setupRequests, joinRequests, joinResponses, holds, discovery, signed, sockets, stored, timers, location, localState, navigations, clock, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

for (const local of ['fresh', 'initialized']) for (const selection of ['target', 'recent']) test(`${selection} Cluster survives ${local} localhost setup and an ambient cookie`, async () => {
  const app = deviceHarness({ local, localSession: true })
  const directory = 'wss://directory.example/custom/socket'
  const selected: ClusterTarget = { kind: 'cluster', clusterId: cluster, directories: [directory] }
  try {
    app.signed.view = { ...membership, members: [gatewayMember(target)] }
    app.stored.set(`vibeface.cluster-state.${cluster}`, JSON.stringify({ genesis }))
    if (selection === 'target') app.stored.set('vibeface.cluster-target', JSON.stringify(selected))
    else app.stored.set('vibeface.last-authenticated', JSON.stringify({ kind: 'cluster', clusterId: cluster, target: selected, path: { address: directory, peer_id: target, mode: 'relay' } }))
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    app.render(); await settle()
    assert.equal(app.render().localSetup?.kind, local === 'fresh' ? 'uninitialized' : 'initialized')
    assert.equal(app.render().ready?.entry.address, directory)
    assert.equal(app.render().ready?.entry.local, undefined)
    assert.deepEqual(app.sockets.map(socket => socket.path), [{ address: directory, mode: 'relay' }])
    assert.deepEqual(JSON.parse(app.stored.get('vibeface.cluster-target') ?? 'null'), selected)
    assert.equal(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? 'null').kind, 'cluster')
    assert.equal(app.calls.includes('local-exchange'), false)
  } finally { app.close() }
})

test('cancelling a connection generation preserves the explicit Cluster and last successful source', async () => {
  const app = deviceHarness(), directory = 'wss://directory.example/custom/socket'
  try {
    app.render(); await settle()
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    await app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: [directory] })
    const savedTarget = app.stored.get('vibeface.cluster-target'), recent = app.stored.get('vibeface.last-authenticated')
    assert.ok(recent)
    const generation = app.render().connectionGeneration()
    app.render().cancelConnection()
    assert.equal(app.render().connectionGeneration(), generation + 1)
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'disconnected')
    assert.equal(app.timers.size, 0)
    assert.equal(app.stored.get('vibeface.cluster-target'), savedTarget)
    assert.equal(app.stored.get('vibeface.last-authenticated'), recent)
    app.render().disconnect()
    assert.equal(app.stored.get('vibeface.cluster-target'), 'null')
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
  } finally { app.close() }
})

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
    await app.render().createLocalCluster('browser', 'test-only-passphrase')
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
    await assert.rejects(app.render().createLocalCluster('browser', 'test-only-passphrase'), /已加入协作集群/)
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
    app.joinResponses.set('status', { kind: 'joined', cluster_id: cluster, peer_id: target })
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

test('refreshing a local Node restores its review without resending a request or accepting it', async t => {
  const app = deviceHarness({ local: 'fresh' }), { receipt } = nodeJoinFixture()
  app.joinResponses.set('status', receipt)
  try {
    app.render()
    await t.waitFor(() => assert.equal(app.render().localJoin?.kind, 'review'))
    assert.deepEqual(JSON.parse(JSON.stringify(app.render().localJoin)), receipt)
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
        const review = { ...receipt.review, ...(mismatch === 'invitation' ? { cluster_id: '6'.repeat(64) } : { new_member: { ...receipt.review.new_member, subject_id: '5'.repeat(64) } }) }
        app.joinResponses.set('request', { ...receipt, review, review_hash: createHash('sha256').update(canonical(review)).digest('hex') })
      }
      await assert.rejects(app.render().pair(invitation, 'node'), /不一致/)
      assert.equal(app.render().localJoin?.kind, 'idle')
      assert.equal(app.joinRequests.some(request => request.action === 'accept'), false)
    } finally { app.close() }
  }
})

test('a lost Node acceptance response is reconciled through its original receipt without automatic resubmission', async t => {
  const app = deviceHarness({ local: 'fresh' }), restored = deviceHarness({ local: 'fresh' }), { invitation, receipt } = nodeJoinFixture()
  const selected: ClusterTarget = { kind: 'cluster', clusterId: cluster, directories: ['wss://directory.example/custom/socket'] }
  const recent = JSON.stringify({ kind: 'cluster', clusterId: cluster, target: selected, path: { address: selected.directories[0], peer_id: target, mode: 'relay' } })
  try {
    app.render(); await settle()
    app.stored.set('vibeface.cluster-target', JSON.stringify(selected))
    app.stored.set('vibeface.last-authenticated', recent)
    app.joinResponses.set('request', receipt)
    await app.render().pair(invitation, 'node')
    assert.equal(app.stored.get('vibeface.cluster-target'), 'null')
    assert.equal(app.stored.get('vibeface.last-authenticated'), recent)
    assert.deepEqual(JSON.parse(JSON.stringify(app.joinRequests.find(request => request.action === 'request')?.params)), { invitation, name: 'node' })
    const hold = deferred(); app.holds.set('local_join:accept', hold)
    const accepting = app.render().acceptLocalJoin(), rejected = assert.rejects(accepting, /response lost/)
    await settle(); hold.reject(new Error('response lost')); await rejected
    assert.equal(app.joinRequests.filter(request => request.action === 'accept').length, 1)
    assert.deepEqual(JSON.parse(JSON.stringify(app.joinRequests.find(request => request.action === 'accept')?.params)), { request_id: receipt.request_id, review_hash: receipt.review_hash })
    app.joinResponses.set('status', { kind: 'uncertain', request_id: receipt.request_id, message: 'check original receipt' })
    await app.render().refreshLocalJoin()
    assert.equal(app.render().localJoin?.kind, 'uncertain')
    app.joinResponses.set('status', { ...receipt, kind: 'pending' })
    await app.render().refreshLocalJoin()
    assert.equal(app.render().localJoin?.kind, 'pending')
    assert.equal(app.joinRequests.filter(request => request.action === 'accept').length, 1)
    assert.equal(app.calls.includes('authenticate'), false)
    for (const [key, value] of app.stored) restored.stored.set(key, value)
    restored.joinResponses.set('status', { ...receipt, kind: 'pending' })
    restored.render()
    await t.waitFor(() => assert.equal(restored.render().localJoin?.kind, 'pending'))
    const resumed = restored.render().localJoin
    assert.equal(resumed?.kind, 'pending')
    assert.equal((resumed?.kind === 'pending' || resumed?.kind === 'review') && resumed.request_id, receipt.request_id)
    assert.equal((resumed?.kind === 'pending' || resumed?.kind === 'review') && resumed.review_hash, receipt.review_hash)
    assert.deepEqual(restored.joinRequests.map(request => request.action), ['status'])
    assert.equal(restored.sockets.length, 0)
    assert.equal(restored.stored.get('vibeface.last-authenticated'), recent)
    assert.equal(restored.render().ready, null)
  } finally { app.close(); restored.close() }
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

test('recovering onto a fresh localhost Node reuses local setup authority without a pasted invitation', async t => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await settle()
    const challenge = { cluster_id: cluster, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
    const challengeHold = deferred(), contextHold = deferred()
    app.holds.set('recovery_challenge', challengeHold); app.holds.set('recovery_context', contextHold)
    challengeHold.resolve({ challenge_id: 'challenge', challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
    const recovering = app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: localEntry, bootstrap: '' })
    const rejected = assert.rejects(recovering, /stop after authority check/)
    await t.waitFor(() => assert.equal(app.calls.includes('recovery_context'), true))
    assert.equal(app.requests.find(request => request.method === 'recovery_context')?.params.capability, localCapability)
    contextHold.reject(new Error('stop after authority check')); await rejected
    assert.equal(app.render().ready, null)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

test('localhost recovery can use a remote fresh Node invitation without sending local setup authority', async t => {
  const app = deviceHarness({ local: 'fresh' }), remote = { ...entry, peer_id: '6'.repeat(64) }, capability = '4'.repeat(64)
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup?.kind, 'uninitialized')
    const challenge = { cluster_id: cluster, owner_public_key: publicKey, subject_id: subject, target_peer_id: remote.peer_id, challenge_id: 'remote-challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
    const challengeHold = deferred(), contextHold = deferred()
    app.holds.set('recovery_challenge', challengeHold); app.holds.set('recovery_context', contextHold)
    challengeHold.resolve({ challenge_id: challenge.challenge_id, challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
    const recovering = app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: remote, bootstrap: JSON.stringify({ url: remote.address, peer_id: remote.peer_id, capability }) })
    const rejected = assert.rejects(recovering, /stop after remote authority check/)
    await t.waitFor(() => assert.equal(app.calls.includes('recovery_context'), true))
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
    await app.render().pair(JSON.stringify({ url: entry.address, peer_id: target, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
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
      await app.render().pair(JSON.stringify({ url: entry.address, peer_id: target, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
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

for (const flow of ['directory', 'recovery']) test(`cancelling after ${flow} response resolves prevents stale state and material publication`, async t => {
  const app = deviceHarness({ saved: flow === 'directory' })
  try {
    app.render(); await settle(); app.render()
    const hold = deferred(); app.holds.set(flow === 'directory' ? 'directory' : 'recovery_context', hold)
    let work: Promise<void>
    if (flow === 'directory') work = app.render().refreshDirectory()
    else {
      const challenge = { cluster_id: cluster, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
      const challengeHold = deferred(); app.holds.set('recovery_challenge', challengeHold)
      challengeHold.resolve({ challenge_id: 'challenge', challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
      work = app.render().beginRecovery({ file: new File(['test-fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', bootstrap: '', entry })
    }
    const rejected = assert.rejects(work, /切换|取消/)
    await t.waitFor(() => assert.equal(app.calls.includes(flow === 'directory' ? 'directory' : 'recovery_context'), true))
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

for (const reason of ['recovery epoch rollback rejected', 'conflicting recovery checkpoint']) test(`recovery stops before confirmation when known membership rejects ${reason}`, async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    const known = JSON.stringify({ genesis, checkpoint: { epoch: 3 } })
    app.stored.set(`vibeface.cluster-state.${cluster}`, known)
    app.signed.mergeError = new Error(reason)
    const challenge = { cluster_id: cluster, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
    const challengeHold = deferred(); app.holds.set('recovery_challenge', challengeHold)
    challengeHold.resolve({ challenge_id: 'challenge', challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
    const context = deferred(); app.holds.set('recovery_context', context)
    context.resolve({ state: { genesis }, view: membership })
    await assert.rejects(app.render().beginRecovery({ file: new File(['test-fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', bootstrap: '', entry }), new RegExp(reason))
    assert.equal(app.render().review, null)
    assert.equal(app.stored.get(`vibeface.cluster-state.${cluster}`), known)
    assert.equal(app.calls.filter(method => ['prepare_recovery', 'recover', 'subscribe'].includes(method)).length, 0)
    assert.equal(app.released(), 1)
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

test('an unsigned authorization error retains the target and retries without clearing trust', async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    app.sockets[0].drop()
    const hold = deferred(); app.holds.set('authenticate', hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    hold.reject(Object.assign(new Error('authorization revoked'), { responded: true })); await settle()
    assert.equal(app.render().ready?.isCurrent(), false)
    assert.equal(app.render().phase, 'reconnecting')
    assert.notEqual(app.stored.get('vibeface.last-authenticated'), 'null')
    assert.ok(app.stored.get(`vibeface.cluster-state.${cluster}`))
    assert.equal(app.timers.size, 1)
    app.render().disconnect(); assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('Cluster candidates continue after a false Noise key and an unsynchronized entry', async () => {
  const app = deviceHarness()
  const peers = ['1', '2', '3'].map(value => value.repeat(64))
  try {
    app.render(); await settle()
    app.discovery.records = peers.map((peerId, index) => ({ peerId, publicKey: index ? publicKey : 'f'.repeat(64), expiresAt: Date.now() / 1000 + 60, paths: [{ address: 'wss://directory.example/custom/socket', mode: 'relay' }] }))
    app.discovery.rejected.add(peers[1])
    await app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: ['wss://directory.example/custom/socket'] })
    const connection = app.render().ready
    assert.equal(connection?.entry.peer_id, peers[2])
    assert.equal(connection?.entry.local, undefined)
    assert.equal(connection?.principal.cluster_id, cluster)
    assert.deepEqual(app.calls.filter(call => call.startsWith('open:')), peers.map(peer => `open:${peer}`))
    assert.equal(app.calls.filter(call => call === 'subscribe').length, 1)
    assert.ok(app.stored.get(`vibeface.cluster-state.${cluster}`))
    assert.equal(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? '{}').kind, 'cluster')
    app.sockets.at(-1)?.drop()
    assert.equal(app.render().phase, 'reconnecting')
    assert.equal(app.timers.size, 1)
    app.render().disconnect()
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

const gatewayMember = (subject_id: string): Member => ({ subject_id, public_key: publicKey, kind: 'node', name: subject_id, node_roles: ['gateway', 'replica'], manage: false })

for (const flow of ['pair', 'recovery']) for (const succeeds of [true, false]) test(`${succeeds ? 'successful' : 'failed'} ${flow} selection replaces the explicit Cluster target and preserves the authenticated refresh source`, async t => {
  const app = deviceHarness(), restored = deviceHarness()
  const previousEntry = { address: 'https://previous.example', peer_id: 'f'.repeat(64) }
  const previousTarget = { kind: 'cluster', clusterId: 'e'.repeat(64), directories: ['wss://previous.example/socket'] } satisfies ClusterTarget
  const previousRecent = { kind: 'cluster', target: previousTarget, path: { address: previousTarget.directories[0], peer_id: previousEntry.peer_id, mode: 'relay' }, clusterId: previousTarget.clusterId }
  const request = { request_id: 'pair', invite_id: 'invite', capability: 'pair-capability', member: { subject_id: subject, public_key: publicKey, kind: 'client', name: 'browser', node_roles: [], manage: false }, nonce: 'nonce', proof: '' }
  const payload = { version: 2, cluster_id: cluster, new_kind: 'client', manage: false, node_roles: [], expires_at_ms: Date.now() + 60000, invite_id: request.invite_id, inviter_subject_id: target, epoch: 1 }
  const invitation = JSON.stringify({ url: entry.address, peer_id: target, invite: { invite: { payload }, capability: request.capability } })
  try {
    app.render(); await settle()
    app.stored.set('vibeface.connections', JSON.stringify([previousEntry]))
    app.stored.set('vibeface.cluster-target', JSON.stringify(previousTarget))
    app.stored.set('vibeface.last-authenticated', JSON.stringify(previousRecent))
    const pending = deferred(), heldMethod = flow === 'pair' ? 'prepare_pair_request' : 'recovery_context'
    app.holds.set(heldMethod, pending)
    if (flow === 'pair') {
      const proposal = [request.request_id, request.invite_id, request.capability, request.member, request.nonce]
      const review = { version: 2, invite_id: request.invite_id, cluster_id: cluster, inviter_subject_id: target, new_member: request.member, proposal_hash: createHash('sha256').update(canonical(proposal)).digest('hex'), parents: [], epoch: 1, expires_at_ms: payload.expires_at_ms }
      const receipt = deferred(); app.holds.set('pair_request', receipt)
      receipt.resolve({ request_id: request.request_id, review, signing: { domain: 'identity-pair-review', payload: canonical(review) } })
      if (succeeds) pending.resolve({ request, signing: { domain: 'identity-pair', payload: canonical(proposal) } })
    } else {
      const challenge = { cluster_id: cluster, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
      const challengeResponse = deferred(); app.holds.set('recovery_challenge', challengeResponse)
      challengeResponse.resolve({ challenge_id: challenge.challenge_id, challenge, signing: { domain: 'identity-recovery-access', payload: canonical(challenge) } })
      const recovered = deferred(); app.holds.set('recover', recovered)
      recovered.resolve({ principal: { cluster_id: cluster }, state: { genesis } })
      if (succeeds) pending.resolve({ state: { genesis }, view: membership })
    }
    const work = flow === 'pair'
      ? app.render().pair(invitation, 'browser')
      : app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', bootstrap: '', entry })
    if (succeeds) {
      await work
      assert.equal(app.stored.get('vibeface.cluster-target'), 'null')
      assert.deepEqual(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? '{}'), previousRecent)
      if (flow === 'pair') {
        assert.equal(app.render().waiting?.entry.peer_id, target)
        await app.render().acceptPair()
      } else {
        const review = app.render().review
        assert.ok(review?.kind === 'recovery')
        await app.render().confirmRecovery(review.members)
      }
      assert.equal(app.render().ready?.entry.peer_id, target)
      assert.deepEqual(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? '{}'), { kind: 'peer', entry, clusterId: cluster })
    } else {
      const rejected = assert.rejects(work, /fixture unavailable/)
      await t.waitFor(() => assert.equal(app.calls.includes(heldMethod), true))
      assert.ok(app.calls.includes(heldMethod))
      pending.reject(new Error('fixture unavailable'))
      await rejected
      assert.equal(app.stored.get('vibeface.cluster-target'), 'null')
      assert.deepEqual(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? '{}'), previousRecent)
      assert.equal(app.render().ready, null)
      assert.equal(app.calls.includes('subscribe'), false)
    }
    for (const [key, value] of app.stored) restored.stored.set(key, value)
    restored.render(); await settle()
    if (succeeds) {
      assert.equal(restored.calls.includes('discover'), false)
      assert.equal(restored.sockets.length, 0)
      assert.deepEqual(JSON.parse(JSON.stringify(restored.render().entries)), [previousEntry, entry])
      assert.equal(restored.stored.get('vibeface.cluster-target'), 'null')
    } else {
      assert.deepEqual(JSON.parse(restored.stored.get('vibeface.cluster-target') ?? '{}'), previousTarget)
      assert.deepEqual(JSON.parse(restored.stored.get('vibeface.last-authenticated') ?? '{}'), previousRecent)
      assert.equal(restored.calls.includes('discover'), true)
      assert.equal(restored.sockets.length, 0)
    }
  } finally { app.close(); restored.close() }
})

test('a persisted Cluster source refreshes through its exact mode before discovery', async () => {
  const first = deviceHarness(), restored = deviceHarness()
  const directory = 'wss://directory.example/custom/socket'
  try {
    first.signed.view = { ...membership, members: [gatewayMember(target)] }
    first.render(); await settle()
    first.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    await first.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: [directory] })
    const recent = JSON.parse(first.stored.get('vibeface.last-authenticated') ?? '{}')
    assert.equal(recent.kind, 'cluster')
    assert.deepEqual(recent.path, { address: directory, peer_id: target, mode: 'relay' })
    for (const [key, value] of first.stored) restored.stored.set(key, value)
    restored.signed.view = first.signed.view
    restored.render(); await settle()
    assert.equal(restored.render().ready?.entry.peer_id, target)
    assert.deepEqual(restored.sockets[0]?.path, { address: directory, mode: 'relay' })
    assert.equal(restored.calls.includes('discover'), false)
    assert.equal(restored.sockets.length, 1)
  } finally { first.close(); restored.close() }
})

for (const change of ['directory', 'cluster']) test(`refresh after an unsuccessful ${change} change preserves the current explicit target`, async () => {
  const first = deviceHarness(), restored = deviceHarness()
  const old = 'wss://old.example/socket', directory = 'wss://directory.example/custom/socket'
  const nextTarget = { kind: 'cluster', clusterId: cluster, directories: [directory] } satisfies ClusterTarget
  try {
    first.signed.view = { ...membership, members: [gatewayMember(target)] }
    first.render(); await settle()
    first.stored.set(`vibeface.cluster-state.${cluster}`, JSON.stringify({ genesis }))
    const previousCluster = change === 'cluster' ? 'f'.repeat(64) : cluster
    first.stored.set('vibeface.last-authenticated', JSON.stringify({ kind: 'cluster', clusterId: previousCluster, target: { ...nextTarget, clusterId: previousCluster, directories: [old] }, path: { address: old, peer_id: target, mode: 'relay' } }))
    await first.render().connectCluster(nextTarget)
    assert.equal(first.render().ready, null)
    assert.equal(first.sockets.length, 0)
    for (const [key, value] of first.stored) restored.stored.set(key, value)
    restored.signed.view = first.signed.view
    restored.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    restored.render(); await settle()
    assert.equal(restored.render().ready?.entry.address, directory)
    assert.deepEqual(restored.sockets.map(socket => socket.entry.address), [directory])
    assert.deepEqual(JSON.parse(restored.stored.get('vibeface.cluster-target') ?? '{}'), nextTarget)
  } finally { first.close(); restored.close() }
})

for (const invalid of ['mode', 'local', 'cluster', 'gateway', 'directory']) test(`a recent Cluster source with invalid ${invalid} cannot bypass current discovery`, async () => {
  const app = deviceHarness(), other = 'e'.repeat(64)
  const directory = 'wss://directory.example/custom/socket', old = 'wss://old.example/socket'
  const targetValue = { kind: 'cluster', clusterId: cluster, directories: [directory] } satisfies ClusterTarget
  try {
    app.signed.view = { ...membership, members: [gatewayMember(target), gatewayMember(other)], revoked: invalid === 'gateway' ? [target] : [] }
    app.stored.set(`vibeface.cluster-state.${cluster}`, JSON.stringify({ genesis }))
    app.stored.set('vibeface.cluster-target', JSON.stringify(targetValue))
    app.stored.set('vibeface.last-authenticated', JSON.stringify({ kind: 'cluster', clusterId: invalid === 'cluster' ? other : cluster, target: targetValue,
      path: { address: invalid === 'directory' ? old : directory, peer_id: target, ...(invalid === 'mode' ? {} : { mode: 'relay' }), ...(invalid === 'local' ? { local: true } : {}) } }))
    app.discovery.records = [{ peerId: other, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    app.render(); await settle()
    assert.equal(app.render().ready?.entry.peer_id, other)
    assert.equal(app.calls.includes('discover'), true)
    assert.deepEqual(app.sockets.map(socket => socket.entry.peer_id), [other])
    assert.deepEqual(app.sockets[0]?.path, { address: directory, mode: 'relay' })
  } finally { app.close() }
})

test('recent and discovered Cluster paths share the 16 Peer and two actual path budget', async () => {
  const app = deviceHarness(), peers = Array.from({ length: 17 }, (_, index) => (index + 1).toString(16).padStart(64, '0'))
  const directory = 'wss://directory.example/custom/socket'
  const targetValue = { kind: 'cluster', clusterId: cluster, directories: [directory] } satisfies ClusterTarget
  try {
    app.signed.view = { ...membership, members: peers.map(gatewayMember) }
    app.stored.set(`vibeface.cluster-state.${cluster}`, JSON.stringify({ genesis }))
    for (const peer of peers) app.discovery.rejected.add(peer)
    app.discovery.records = peers.map(peerId => ({ peerId, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [
      { address: directory, mode: 'relay' }, { address: directory, mode: 'direct' }, { address: 'wss://extra.example/socket', mode: 'direct' },
    ] }))
    app.render(); await settle()
    app.stored.set('vibeface.last-authenticated', JSON.stringify({ kind: 'cluster', target: targetValue, clusterId: cluster, path: { address: directory, peer_id: peers[0], mode: 'relay' } }))
    await app.render().connectCluster(targetValue)
    const attempted = new Map<string, Set<string>>()
    for (const socket of app.sockets) {
      assert.ok(socket.path)
      const paths = attempted.get(socket.entry.peer_id) ?? new Set<string>()
      paths.add(`${socket.path.mode}:${socket.path.address}`); attempted.set(socket.entry.peer_id, paths)
    }
    assert.equal(attempted.size, 16)
    assert.equal(app.sockets.length, 32)
    assert.ok([...attempted.values()].every(paths => paths.size === 2))
    assert.deepEqual([...attempted.get(peers[0]) ?? []], [`relay:${directory}`, `direct:${directory}`])
  } finally { app.close() }
})

test('a failed candidate retains signed revocation and prevents the next stale direct path', async () => {
  const app = deviceHarness(), peers = ['1', '2', '3'].map(value => value.repeat(64))
  const directory = 'wss://directory.example/custom/socket'
  try {
    app.signed.view = { ...membership, members: peers.map(gatewayMember) }
    app.stored.set(`vibeface.cluster-state.${cluster}`, JSON.stringify({ genesis }))
    app.render(); await settle()
    const gate = deferred(); app.holds.set('authenticate', gate)
    app.discovery.records = peers.map((peerId, index) => ({ peerId, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: index === 1 ? 'wss://revoked.example/socket' : directory, mode: index === 1 ? 'direct' : 'relay' }] }))
    const connecting = app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: [directory] })
    await settle()
    assert.equal(app.sockets[0]?.entry.peer_id, peers[0])
    app.signed.view = { ...app.signed.view, revoked: [peers[1]] }
    app.signed.rejectNextAuthentication = true
    app.holds.delete('authenticate'); gate.resolve({})
    await connecting
    assert.deepEqual(app.sockets.map(socket => socket.entry.peer_id), [peers[0], peers[2]])
    assert.equal(app.render().ready?.entry.peer_id, peers[2])
    assert.ok(app.stored.get(`vibeface.cluster-state.${cluster}`))
  } finally { app.close() }
})

for (const stage of ['open', 'authenticate', 'subscribe']) test(`a Cluster announcement expiring during ${stage} cannot publish Ready`, async () => {
  const app = deviceHarness(), directory = 'wss://directory.example/custom/socket'
  try {
    app.render(); await settle()
    const gate = deferred(); app.holds.set(stage, gate)
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: app.clock.now / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    const connecting = app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: [directory] })
    await settle()
    app.clock.now += 120000
    gate.resolve({}); await connecting
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'reconnecting')
    assert.equal(app.sockets.length, 1)
    assert.equal(app.stored.get('vibeface.last-authenticated') ?? 'null', 'null')
  } finally { app.close() }
})

test('a Client revocation learned during subscribe prevents Cluster Ready', async () => {
  const app = deviceHarness(), directory = 'wss://directory.example/custom/socket'
  try {
    app.render(); await settle()
    const gate = deferred(); app.holds.set('subscribe', gate)
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: app.clock.now / 1000 + 60, paths: [{ address: directory, mode: 'relay' }] }]
    const connecting = app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: [directory] })
    await settle()
    app.signed.view = { ...membership, revoked: [subject] }
    gate.resolve({}); await connecting
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.equal(app.timers.size, 0)
    assert.equal(app.sockets.length, 1)
  } finally { app.close() }
})

test('cancelled discovery and an old attempt cannot release the newer probe', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    const first = deferred(), second = deferred(), auth = deferred()
    app.holds.set('discover', first)
    const opening = app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: ['https://one.example'] })
    await settle()
    app.holds.set('discover', second)
    const replacing = app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: ['https://two.example'] })
    await settle()
    assert.equal(app.discovery.signals[0].aborted, true)
    first.resolve({}); await opening
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: 'wss://two.example/api/network/ws', mode: 'relay' }] }]
    app.holds.set('authenticate', auth)
    second.resolve({}); await settle()
    app.sockets.at(-1)?.drop()
    assert.equal(app.timers.size, 0)
    auth.resolve({}); await replacing
    assert.equal(app.render().ready, null)
    assert.equal(app.timers.size, 1)
    app.render().disconnect()
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('a verified client revocation retains signed state and stops Cluster retries', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    app.signed.view = { ...membership, revoked: [subject] }
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, paths: [{ address: 'wss://directory.example/api/network/ws', mode: 'relay' }] }]
    await app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: ['https://directory.example'] })
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.equal(app.timers.size, 0)
    assert.ok(app.stored.get(`vibeface.cluster-state.${cluster}`))
    assert.notEqual(app.stored.get('vibeface.cluster-target'), 'null')
  } finally { app.close() }
})

test('late nodes responses cannot reintroduce a member revoked by a newer signed directory', async () => {
  const app = deviceHarness({ saved: true })
  const other = 'e'.repeat(64)
  try {
    app.signed.view = { ...membership, members: [target, other].map(subject_id => ({ subject_id, public_key: publicKey, kind: 'node', name: subject_id, node_roles: ['gateway', 'replica'], manage: false } satisfies Member)) }
    app.render(); await settle()
    const connection = app.render().ready
    assert.ok(connection)
    const hold = deferred(); app.holds.set('rpc', hold)
    const reading = connection.rpc('nodes'); await settle()
    app.signed.view = { ...app.signed.view, revoked: [other] }
    await app.render().refreshDirectory()
    hold.resolve({ nodes: [target, other].map(peer_id => ({ peer_id, roles: ['gateway', 'replica'], online: true, revoked: false })) })
    const response = JSON.parse(JSON.stringify(await reading))
    assert.deepEqual(response.nodes.map((node: { peer_id: string }) => node.peer_id), [target])
    app.signed.view = { ...app.signed.view, epoch: 2 }
    await assert.rejects(app.render().refreshDirectory(), /授权已变化/)
    assert.equal(connection.isCurrent(), false)
    assert.equal(app.render().phase, 'reconnecting')
  } finally { app.close() }
})

test('Cluster discovery verifies signed records and limits source and path budgets', async () => {
  const sources = Array.from({ length: 8 }, (_, index) => `wss://directory${index}.example/custom/socket`)
  const pages = sources.map(() => Array.from({ length: 8 }, () => {
    const keys = generateKeyPairSync('ed25519')
    const publicKey = keys.publicKey.export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex')
    const peer = createHash('sha256').update(Buffer.from(publicKey, 'hex')).digest('hex')
    const now = Math.floor(Date.now() / 1000)
    const record = { peer_id: peer, public_key: publicKey, cluster: { cluster_id: cluster, services: ['peer', 'client'] }, direct_urls: ['ws://127.0.0.1/private/socket'], relay_urls: ['wss://unconfigured.example/socket'], issued_at: now, expires_at: now + 180, signature: '' }
    const hash = createHash('sha256').update(canonical(record)).digest('hex')
    record.signature = signBytes(null, Buffer.from(`vibeface:message:v1:reachability-v2:${hash}`), keys.privateKey).toString('hex')
    return record
  }))
  let active = 0, peak = 0, hold = false
  class Socket {
    listeners = new Map<string, (event: unknown) => void>()
    closed = false
    address: string
    constructor(address: URL) { this.address = address.href; active++; peak = Math.max(peak, active); if (!hold) queueMicrotask(() => this.listeners.get('message')?.({ data: JSON.stringify({ type: 'welcome' }) })) }
    addEventListener(type: string, listener: (event: unknown) => void) { this.listeners.set(type, listener) }
    send(value: string) { assert.equal(JSON.parse(value).type, 'lookup_cluster'); queueMicrotask(() => this.listeners.get('message')?.({ data: JSON.stringify({ type: 'cluster_records', records: pages[sources.indexOf(this.address)], truncated: false }) })) }
    close() { if (this.closed) return; this.closed = true; active--; this.listeners.get('close')?.({}) }
  }
  const source = readFileSync(new URL('../../../transport.js', import.meta.url), 'utf8').replace(/^import[^\n]+\n/, '').replace(/\bexport /g, '')
  const transport = new Script(`${source}\n({clusterRecord, discoverCluster, validEndpoint})`).runInNewContext({ TextEncoder, TextDecoder, URL, Uint8Array, crypto: globalThis.crypto, location: { protocol: 'https:' }, WebSocket: Socket, setTimeout, clearTimeout,
    verify(key: string, domain: string, payload: Uint8Array, signature: string) {
      const publicKey = createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), Buffer.from(key, 'hex')]), format: 'der', type: 'spki' })
      const hash = createHash('sha256').update(payload).digest('hex')
      assert.equal(verifyBytes(null, Buffer.from(`vibeface:message:v1:${domain}:${hash}`), publicKey, Buffer.from(signature, 'hex')), true)
    },
  })
  const target = { clusterId: cluster, directories: sources }
  const records = await transport.discoverCluster(target)
  assert.equal(records.length, 16)
  assert.ok(peak <= 2)
  assert.equal(active, 0)
  for (const source of sources) assert.equal(records.filter((record: { paths: ClusterPath[] }) => record.paths.some(entry => entry.address === source)).length, 2)
  assert.ok(records.every((record: { paths: ClusterPath[] }) => record.paths.length <= 2 && record.paths.every(entry => entry.mode === 'relay' && !Object.hasOwn(entry, 'local') && sources.includes(entry.address))))
  const repeated = generateKeyPairSync('ed25519')
  const repeatedKey = repeated.publicKey.export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex')
  const repeatedPeer = createHash('sha256').update(Buffer.from(repeatedKey, 'hex')).digest('hex')
  const issued = Math.floor(Date.now() / 1000)
  for (const [index, ttl] of [30, 120].entries()) {
    const record = { peer_id: repeatedPeer, public_key: repeatedKey, cluster: { cluster_id: cluster, services: ['peer', 'client'] }, direct_urls: ['wss://direct0.example/socket', 'wss://direct1.example/socket', 'wss://direct2.example/socket'], relay_urls: [], issued_at: issued, expires_at: issued + ttl, signature: '' }
    const hash = createHash('sha256').update(canonical(record)).digest('hex')
    record.signature = signBytes(null, Buffer.from(`vibeface:message:v1:reachability-v2:${hash}`), repeated.privateKey).toString('hex')
    pages[index][0] = record
  }
  const merged = await transport.discoverCluster({ ...target, directories: sources.slice(0, 2) })
  assert.equal(merged.find((record: { peerId: string }) => record.peerId === repeatedPeer)?.expiresAt, issued + 30)
  const visited = new Set<string>()
  for (let attempt = 0; attempt < 5; attempt++) {
    const round = await transport.discoverCluster({ ...target, directories: sources.slice(0, 2) }, { trustedPeers: [repeatedPeer], attempt })
    const candidate = round.find((record: { peerId: string }) => record.peerId === repeatedPeer)
    assert.ok(candidate)
    assert.ok(candidate.paths.length <= 2)
    for (const entry of candidate.paths) visited.add(entry.address)
  }
  assert.deepEqual([...visited].sort(), [...sources.slice(0, 2), ...pages[0][0].direct_urls].sort())
  assert.equal(transport.validEndpoint(sources[0]), sources[0])
  const record = pages[0][0]
  await assert.rejects(transport.clusterRecord(record, 'e'.repeat(64)), /集群/)
  await assert.rejects(transport.clusterRecord({ ...record, peer_id: 'f'.repeat(64) }, cluster), /Peer ID/)
  await assert.rejects(transport.clusterRecord({ ...record, expires_at: 1 }, cluster), /过期/)
  await assert.rejects(transport.clusterRecord({ ...record, signature: '00'.repeat(64) }, cluster))
  await assert.rejects(transport.clusterRecord({ ...record, direct_urls: ['wss://large.example/' + 'x'.repeat(8192)] }, cluster), /公告格式/)
  hold = true
  const controller = new AbortController()
  const pending = transport.discoverCluster(target, { signal: controller.signal })
  await settle(); controller.abort()
  await assert.rejects(pending, /取消/)
  assert.equal(active, 0)
})

test('successful bootstrap releases local material without disconnecting the authenticated session', async () => {
  const app = deviceHarness({ bootstrap: true })
  try {
    app.render(); await settle()
    await app.render().pair(JSON.stringify({ url: entry.address, peer_id: target, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
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


async function routeHarness({ invalid = '', hold = false, secure = false, legacy = false, local = false, localFail = false, withCluster = false, endpoint = '', untrusted = false } = {}) {
  await initCrypto({ module_or_path: readFileSync(new URL('../../../generated/vibeface_secure_bg.wasm', import.meta.url)) })
  const client = Identity.generate(), targetKey = Identity.generate()
  const targetUrl = endpoint || `${secure ? 'wss' : 'ws'}://${local ? 'entry.example/api/local/ws' : legacy ? 'entry.example/api/network/ws' : 'target.example/custom/network'}`
  const now = Math.floor(Date.now() / 1000)
  const advertisedCluster = withCluster ? { cluster_id: cluster, services: ['peer', 'client'] } : null
  const record = { peer_id: targetKey.peerId(), public_key: targetKey.publicKeyHex(), cluster: advertisedCluster, direct_urls: [untrusted ? 'ws://192.168.1.10/private/socket' : 'ws://offline.example/path'], relay_urls: untrusted ? ['wss://unconfigured.example/socket'] : [targetUrl], issued_at: now, expires_at: now + 120, signature: '' }
  record.signature = targetKey.sign('reachability-v2', new TextEncoder().encode(canonical(record)))
  if (invalid === 'signature') record.relay_urls = ['ws://attacker.example/path']
  if (invalid === 'expired') record.expires_at = now - 1
  if (invalid === 'target') record.peer_id = 'f'.repeat(64)
  if (invalid === 'cluster') record.cluster = { cluster_id: 'e'.repeat(64), services: ['client'] }
  if (invalid === 'domain') record.signature = targetKey.sign('reachability', new TextEncoder().encode(canonical({ ...record, signature: '' })))
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
        if (message.type === 'lookup_cluster') { queueMicrotask(() => this.emit(JSON.stringify({ type: 'cluster_records', records: [record], truncated: false }))); return }
        if (message.type === 'lookup') {
          const { cluster: _cluster, ...oldRecord } = record
          if (!hold) queueMicrotask(() => this.emit(JSON.stringify({ type: 'record', record: legacy ? null : invalid === 'fields' ? oldRecord : record })))
          return
        }
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
  const initialization = deferred()
  const runtime = new Script(`${source}\nidentity = testIdentity; ({ connectDevice, connectClusterPath, discoverCluster, delayIdentity() { identity = undefined; initializing = initialization.then(() => { identity = testIdentity; }); } });`).runInNewContext({
    init: initCrypto, Identity, Handshake, verify, testIdentity: client, TextEncoder, TextDecoder, Uint8Array, ArrayBuffer,
    WebSocket: RecordSocket, URL, crypto: globalThis.crypto, location: { protocol: secure ? 'https:' : 'http:', origin: `${secure ? 'https' : 'http'}://entry.example` }, AbortController, setTimeout, clearTimeout, isSecureContext: true, indexedDB: {}, initialization: initialization.promise,
  })
  return {
    entry: { address: `${secure ? 'https' : 'http'}://entry.example`, peer_id: targetKey.peerId(), ...(local ? { local: true } : {}) }, controls, routes, targetUrl, initialization,
    connect(signal?: AbortSignal) { return runtime.connectDevice(this.entry, { signal, onClose() { closedEvents++ } }) },
    connectPath(mode: 'direct' | 'relay', address = targetUrl, signal?: AbortSignal) { return runtime.connectClusterPath({ address, peer_id: targetKey.peerId(), mode }, { signal, onClose() { closedEvents++ } }) },
    discover() { return runtime.discoverCluster({ clusterId: cluster, directories: [targetUrl] }) },
    delayIdentity() { runtime.delayIdentity() },
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

for (const invalid of ['signature', 'expired', 'target', 'cluster', 'domain', 'fields']) test(`automatic transport rejects a ${invalid} directory record without opening its candidates`, async () => {
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

test('identical saved entries collapse into one target without losing its identity', async () => {
  const app = deviceHarness()
  try {
    app.stored.set('vibeface.connections', JSON.stringify([entry, entry]))
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

test('ordinary Peer lookup accepts the same v2 Cluster declaration as Cluster discovery', async () => {
  const app = await routeHarness({ withCluster: true })
  try {
    const channel = await app.connect()
    assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
    assert.equal(app.routes.includes(app.targetUrl), true)
    channel.close(); await settle()
    assert.equal(app.open(), 0)
  } finally { app.close() }
})

test('an unknown Cluster Peer only dials the explicit directory that returned it', async () => {
  const app = await routeHarness({ withCluster: true, untrusted: true })
  try {
    const candidates = await app.discover()
    assert.equal(candidates.length, 1)
    const [candidate] = candidates
    assert.equal(candidate.peerId, app.entry.peer_id)
    assert.deepEqual(JSON.parse(JSON.stringify(candidate.paths)), [{ address: app.targetUrl, mode: 'relay' }])
    const channel = await app.connectPath(candidate.paths[0].mode, candidate.paths[0].address)
    assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
    channel.close(); await settle()
    assert.deepEqual(app.routes, [app.targetUrl])
    assert.deepEqual(app.controls.map(message => message.type), ['lookup_cluster', 'dial'])
    assert.equal(app.open(), 0)
  } finally { app.close() }
})

for (const mode of ['direct', 'relay'] satisfies ClusterPath['mode'][]) test(`Cluster ${mode} performs one actual path with no lookup or fallback`, async () => {
  for (const succeeds of [true, false]) {
    const app = await routeHarness()
    try {
      const address = succeeds ? app.targetUrl : 'ws://entry.example/api/network/ws'
      if (succeeds) {
        const channel = await app.connectPath(mode, address)
        assert.equal((await channel.request('info')).peer_id, app.entry.peer_id)
        channel.close(); await settle()
      } else await assert.rejects(app.connectPath(mode, address))
      assert.deepEqual(app.routes, [address])
      assert.deepEqual(app.controls.map(message => message.type), [mode === 'relay' ? 'dial' : 'direct'])
      assert.equal(app.maximum(), 1)
      assert.equal(app.open(), 0)
    } finally { app.close() }
  }
})

for (const endpoint of ['ws://target.example/', 'wss://target.example/', 'wss://target.example/custom/socket']) test(`automatic and exact paths preserve the complete endpoint ${endpoint}`, async () => {
  const app = await routeHarness({ endpoint })
  try {
    app.entry.address = endpoint
    const auto = await app.connect()
    assert.equal((await auto.request('info')).peer_id, app.entry.peer_id)
    auto.close(); await settle()
    const exact = await app.connectPath('relay')
    assert.equal((await exact.request('info')).peer_id, app.entry.peer_id)
    exact.close(); await settle()
    assert.ok(app.routes.every(address => address === endpoint))
    assert.equal(app.open(), 0)
  } finally { app.close() }
})

test('cancellation during identity initialization opens no Cluster or Peer socket', async () => {
  for (const exact of [false, true]) {
    const app = await routeHarness(), controller = new AbortController()
    try {
      app.delayIdentity()
      const connecting = exact ? app.connectPath('relay', app.targetUrl, controller.signal) : app.connect(controller.signal)
      const rejected = assert.rejects(connecting, /取消/)
      controller.abort(); app.initialization.resolve({})
      await rejected; await settle()
      assert.deepEqual(app.routes, [])
      assert.deepEqual(app.controls, [])
      assert.equal(app.open(), 0)
    } finally { app.close() }
  }
})
