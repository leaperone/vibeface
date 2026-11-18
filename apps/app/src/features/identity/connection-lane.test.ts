import assert from 'node:assert/strict'
import test from 'node:test'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import type { ConnectionEntry, MembershipView } from '@vibeface/client/lib/node.ts'
import { canonical } from '@vibeface/client/lib/node.ts'
import type { Platform } from '@vibeface/client/lib/platform.ts'
import { primaryLane, shouldAutoConnect } from '@vibeface/client/identity/connection-lane.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { spentInviteText } from '@vibeface/client/lib/errors.ts'
import { joinLink, joinTicket } from '@vibeface/client/lib/transport.ts'
import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'

test('one saved entry connects itself, and several entries stay listed', () => {
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 1 }), true)
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 2 }), false)
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 0 }), false)
  assert.equal(shouldAutoConnect({ hasInvitation: true, savedCount: 1 }), false)
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 3, local: 'none' }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 0, local: 'none' }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1, local: 'none' }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2, local: 'none' }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1, local: 'open' }), 'local')
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 0, local: 'open' }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2, local: 'none', removed: true }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2, local: 'open', removed: true }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1, local: 'join' }), 'invite', 'a computer in use leads a browser not in it to the join form, not to its saved address')
})

const target = 'a'.repeat(64), network = 'b'.repeat(64), subject = 'c'.repeat(64)
const entry = { kind: 'gateway', address: 'https://node.example', device_id: target } satisfies ConnectionEntry
const localEntry = { kind: 'gateway', address: 'http://localhost:4790', device_id: target } satisfies ConnectionEntry
const localCapability = 'f'.repeat(64)
const localInvitation = { url: localEntry.address, device_id: target, capability: localCapability }
const localLaunch = { url: 'http://127.0.0.1:49781', device_id: target, token: 'e'.repeat(64) }
const genesis = { version: 1, network_id: network, first_client: { device_id: subject, kind: 'client', name: 'browser', manage: true }, first_node: { device_id: target, kind: 'node', name: 'node', manage: false }, signature: '5'.repeat(128) } as const
const membership = { network_id: network, recovery_key: network, members: [], revoked: [], heads: [], epoch: 1, attesters: [], pending: null, stopped: [] }
const inviteId = '1'.repeat(32), joinNonce = '9'.repeat(64)
const joinRequest = (status: string, extra: Record<string, unknown> = {}) => ({ join_id: inviteId, status, network_id: network, invite_id: inviteId, device_id: subject, kind: 'client', name: 'browser', commit: 'd'.repeat(64), admin_id: status === 'confirming' || status === 'joined' ? target : null, admin_nonce: status === 'confirming' || status === 'joined' ? '2'.repeat(64) : null, joiner_nonce: null, reason: null, expires_at_ms: Date.now() + 60000, ...extra })
const nextRecoveryKey = '4'.repeat(64)
function recoveryCheckpointFixture(members: MembershipView['members']) {
  return { version: 1, network_id: network, epoch: 2, recovery_key: nextRecoveryKey, directory: { members: [...members].sort((a, b) => a.device_id.localeCompare(b.device_id)), retired: [] }, heads: [], issued_at_ms: 1000, signature: '3'.repeat(128) }
}
const checkpointHash = (checkpoint: unknown) => createHash('sha256').update(canonical(checkpoint)).digest('hex')
const deferred = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (reason: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settle = () => new Promise(setImmediate)
const plain = (value: unknown) => JSON.parse(JSON.stringify(value))
const until = async (ready: () => boolean) => {
  const deadline = Date.now() + 5000
  while (!ready()) {
    if (Date.now() > deadline) throw new Error('condition not reached within 5s')
    await new Promise(resolve => setTimeout(resolve, 1))
  }
}

const remote = '6'.repeat(64), freshKey = '7'.repeat(64)
function deviceHarness({ joining = '', saved = false, local = '', invitation = '', localSession = false, management = false, fragment = '', storage = new Map<string, string>(), tabStorage = new Map<string, string>(), removed = false, origin = '', entryAddrs = [] as string[] } = {}) {
  const hooks = hookHarness(true), { effects, afterRender } = hooks
  const timers = new Map<number, () => void>()
  const stored = storage, calls: string[] = [], navigations: string[] = []
  const requests: { method: string; params: Record<string, unknown> }[] = []
  const setupRequests: { path: string; options: RequestInit }[] = []
  const joinStatus = { value: joining ? joinRequest(joining) : joinRequest('pending') } as { value: Record<string, unknown> }
  const managed = { value: [] as Record<string, unknown>[] }
  const inviteState = { value: 'open' }
  const holds = new Map<string, ReturnType<typeof deferred>>()
  const signatures: string[] = []
  const directory: { state: { genesis: typeof genesis; checkpoints: unknown[]; events: unknown[] }; view: MembershipView; now_ms: number } = {
    state: { genesis, checkpoints: [], events: [] },
    view: { ...membership, members: [{ device_id: subject, kind: 'client', name: 'browser', manage: management }, { device_id: target, kind: 'node', name: 'node', manage: false }] },
    now_ms: Date.now(),
  }
  const views = new Map<unknown, MembershipView>()
  const receipt = () => { views.set(directory.state, directory.view); return { ...directory, heads: {} } }
  const sockets: { entry: ConnectionEntry; close(): void; drop(): void }[] = []
  // As the Node sends it.
  const sessionEntry = { address: 'https://node.example', device_id: target, local: true }
  const localState: { enabled: boolean; setup?: unknown } = { enabled: true }
  const pageOrigin = origin || (local ? localEntry.address : entry.address)
  const hash = invitation ? `#${invitation}` : fragment, pathname = invitation ? '/join' : '/'
  const location = { hash, pathname, href: `${pageOrigin}${pathname}${hash}`, origin: pageOrigin, assign(url: string) { navigations.push(url) } }
  const hashListeners = new Set<() => void>()
  const openHash = (path: string, next: string) => { Object.assign(location, { pathname: path, hash: next, href: `${pageOrigin}${path}${next}` }); hashListeners.forEach(listener => listener()) }
  let nextTimer = 0
  const react = {
    ...hooks.react, useCallback: (fn: unknown) => fn,
    useMemo(read: () => unknown) { hooks.next(); return read() },
  }
  if (joining) stored.set('vibeface.pending-join', JSON.stringify({ entry: { address: entry.address, device_id: entry.device_id }, join_id: inviteId, invite_id: inviteId, nonce: joinNonce, name: 'browser', ...(joining === 'pending' ? {} : { pinned: { network_id: network, admin_id: target, admin_nonce: '2'.repeat(64) } }) }))
  if (saved) stored.set('vibeface.connections', JSON.stringify([entry]))
  // A cookie-bound local session only exists after a launch on this origin, which the browser remembers.
  if (localSession) stored.set('vibeface.local-origin', 'true')
  let key = subject
  let initialized = local !== 'fresh'
  const transport = {
    validTarget: (value: string) => value, validEndpoint: (value: string) => value, joinTicket, joinLink,
    initClient: async () => ({ deviceId: key }), signClient: async (purpose: string) => { signatures.push(purpose); return 'signature' },
    replaceClient: async () => { calls.push('replace-key'); key = freshKey; return { deviceId: key } },
    parseJoinLink: async () => ({ entryId: target, entryAddrs, inviteId, inviteHash: 'h'.repeat(64), secret: 'one-time-secret' }),
    async connectDevice(candidate: ConnectionEntry, options: { onClose?(cause: Error): void; signal?: AbortSignal }) {
      calls.push(`open:${candidate.device_id}`)
      let closed = false
      let authorized = false
      const close = () => { if (closed) return; closed = true; options.onClose?.(new Error('connection lost')) }
      sockets.push({ entry: candidate, close, drop: close })
      options.signal?.addEventListener('abort', close)
      const opening = holds.get(candidate.kind === 'lan' ? 'open' : `open:${candidate.address}`) ?? holds.get('open')
      if (opening) await opening.promise
      return { deviceId: candidate.device_id, close, async request(method: string, params: Record<string, unknown> = {}) {
        if (method === 'log') return { accepted: 1 }
        calls.push(method)
        requests.push({ method, params })
        if (holds.has(method)) {
          const result = await holds.get(method)?.promise
          if (method === 'recover') { initialized = true; authorized = true }
          if (method === 'authenticate' && result && typeof result === 'object' && 'principal' in result) authorized = result.principal !== null
          return result
        }
        if (method === 'info') return { device_id: candidate.device_id, name: authorized ? 'node' : 'Node', version: 'test', initialized, network_id: initialized && authorized ? network : null }
        if (method === 'recover') { initialized = true; authorized = true; return {} }
        if (method === 'authenticate' && localSession) { initialized = true; authorized = true; return { principal: { network_id: network } } }
        if (method === 'authenticate' && removed && key === subject) return { principal: null, initialized: true, network_id: network, revoked: true }
        if (method === 'authenticate') { initialized = true; authorized = true }
        if (method === 'local_access') { localState.enabled = false; return { enabled: false } }
        if (method === 'join_check' || method === 'node_join_check') return { invite: inviteState.value }
        if (method === 'invite') return { entry: 'fixture-entry' }
        if (method === 'join') return joinRequest('pending', { device_id: params.public_key })
        if (method === 'join_status' && !params.join_id) return { requests: managed.value }
        if (method === 'approve') return joinRequest('confirming', { device_id: remote, admin_id: subject, admin_nonce: params.admin_nonce })
        if (method === 'directory' || method === 'approve_recovery') return receipt()
        if (method === 'join_status') return params.reveal ? { ...joinStatus.value, joiner_nonce: params.reveal } : joinStatus.value.status === 'joined' ? { ...joinStatus.value, state: { genesis } } : joinStatus.value
        if (method === 'recovery_state') return { state: { genesis, checkpoints: [], events: [] }, view: membership, heads: {} }
        return {}
      } }
    },
  }
  let verifications = 0, released = 0
  const secure = {
    verifyAuthentication: () => { verifications++; return { auth: { local_auto_access: true }, principal: { source: localSession && !management ? 'local_session' : 'membership', network_id: network, device_id: subject, epoch: 1, can_manage: management }, view: membership } },
    verifyRemoval: () => false,
    verifiedView: (state?: unknown) => views.get(state) ?? directory.view,
    openPackage: async () => ({ networkId: network, material: { genesis, checkpoints: [], events: [] }, root: { deviceId: () => network, sign: () => 'signature' }, release() { released++ } }),
    Identity: { generate: () => ({ deviceId: () => nextRecoveryKey, secretHex: () => 'private-test-key', sign: () => 'signature', free() { released++ } }) },
    createGenesis: () => JSON.stringify(genesis), joinCommit: () => 'd'.repeat(64), shortCode: () => '123456', ticketEncode: () => 'vibeface1test',
    governancePayload: () => new Uint8Array(), inviteSigningPayload: () => new Uint8Array(),
    sealPackage: () => new Uint8Array([1, 2, 3]),
    verifyState: (state: unknown = directory.state) => ({ free() {}, viewJson: () => JSON.stringify(views.get(state) ?? directory.view), governanceTemplate: (issuer: string, action: string) => JSON.stringify({ issuer, action: JSON.parse(action) }), createRecoveryCheckpoint: (_root: unknown, _next: string, members: string) => JSON.stringify(recoveryCheckpointFixture(JSON.parse(members))), createKeyRotation: () => JSON.stringify({ epoch: 2 }) }),
  }
  const timer = (run: () => void) => { const id = ++nextTimer; timers.set(id, run); return id }
  const sandbox = createContext({
    Date,
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    sessionStorage: { getItem: (key: string) => tabStorage.get(key) ?? null, setItem: (key: string, value: string) => tabStorage.set(key, value), removeItem: (key: string) => tabStorage.delete(key) },
    location, addEventListener: (type: string, listener: () => void) => { if (type === 'hashchange') hashListeners.add(listener) }, removeEventListener: (_type: string, listener: () => void) => hashListeners.delete(listener),
    history: { replaceState(_state: unknown, _title: string, url: URL) { location.hash = url.hash } }, URL, URLSearchParams, AbortController, TextEncoder, TextDecoder, Uint8Array, Error, btoa, atob, crypto: globalThis.crypto,
    async fetch(path: string, options: RequestInit) {
      if (path === '/api/local/info') calls.push('local-info')
      if (path === '/api/local/info') return localSession
        ? { ok: true, json: async () => ({ enabled: localState.enabled, has_session: localState.enabled, device_id: subject, network_id: network, entry: sessionEntry }) }
        : { ok: false, status: 404 }
      if (path === '/api/local/exchange') {
        assert.equal(localSession, true)
        assert.equal(options.credentials, 'same-origin')
        assert.equal(options.redirect, 'error')
        assert.deepEqual(JSON.parse(String(options.body)), { token: 'f'.repeat(64), node_id: target, device_id: subject })
        calls.push('local-exchange')
        return { ok: true, json: async () => ({ network_id: network, entry: sessionEntry }) }
      }
      calls.push('local_setup'); setupRequests.push({ path, options })
      const value = holds.has('local_setup') ? await holds.get('local_setup')?.promise : localState.setup ?? (local === 'fresh' ? { kind: 'uninitialized', invitation: localInvitation } : { kind: 'initialized', removed: false, entry: localEntry })
      return { status: 200, ok: true, json: async () => value }
    },
    setTimeout: timer, clearTimeout: (id: number) => timers.delete(id), setInterval: timer, clearInterval: (id: number) => timers.delete(id),
  })
  const sourceRoot = fileURLToPath(new URL('../../', import.meta.url))
  const clientRoot = fileURLToPath(new URL('../../../../../packages/client/src/', import.meta.url))
  const load = moduleLoader(sandbox, (specifier, filename) => {
    if (specifier === 'react') return react
    if (specifier === 'react/jsx-runtime') return { jsx: (_type: unknown, props: unknown) => props }
    if (specifier.endsWith('/lib/transport.ts')) return transport
    if (specifier.endsWith('/lib/secure.ts')) return { secureKit: () => secure, recoveryCode: async () => 'ABCDEF12', recoveryFileName: (code: string) => `vibeface-recovery-${code}.json` }
    const path = specifier.startsWith('@/') ? resolve(sourceRoot, specifier.slice(2)) : specifier.startsWith('@vibeface/client/') ? resolve(clientRoot, specifier.slice(17)) : resolve(dirname(filename), specifier)
    return load(extname(path) ? path : `${path}.ts`)
  })
  const Provider = load(resolve(clientRoot, 'identity/context.tsx')).DeviceProvider
  const platform = {
    reserveDeliveryIntent: async () => { throw new Error('unexpected delivery reservation') },
    secure: {
      ticketDecode: () => JSON.stringify({ entry_id: target, invite_id: inviteId }),
      verify(rootKey: string, domain: string, _payload: Uint8Array, signature: string) { assert.equal(rootKey, network); assert.equal(domain, 'identity-recovery'); assert.equal(signature, '3'.repeat(128)) },
    }, transport, storage: sandbox.localStorage, page: load(resolve(sourceRoot, 'lib/local-page.ts')).localPage, self: '这个浏览器', saveRecovery(_bytes: Uint8Array, _name: string) {}, recoveryStorage: undefined as Platform['recoveryStorage'], joinTabs: undefined as Platform['joinTabs'], erase: undefined as Platform['erase'] }
  let tabReceive: ((serialized: string) => void) | undefined
  const enableTabs = () => { platform.joinTabs = (_name, handlers) => { tabReceive = value => handlers.message(JSON.parse(value)); const stop = handlers.lead(); return { post() {}, close() { tabReceive = undefined; stop?.() } } } }
  let restarts = 0
  assert.equal(typeof Provider, 'function')
  type Device = ReturnType<typeof import('@vibeface/client/identity/context.tsx').useDevice>
  const render = (): Device => {
    hooks.reset()
    assert.ok(typeof Provider === 'function')
    const props = Provider({ platform, children: null, restart: () => { restarts++ } })
    afterRender.splice(0).forEach(run => run())
    return props.value
  }
  return { render, platform, enableTabs, broadcast: (value: unknown) => tabReceive?.(JSON.stringify(value)), secure, calls, requests, setupRequests, joinStatus, inviteState, managed, holds, directory, receipt, signatures, sockets, stored, timers, location, openHash, localState, navigations, restarts: () => restarts, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

const notMember = { principal: null, initialized: true, network_id: network, revoked: false }

test('sync watermarks follow their network across entry changes and reconnects', async () => {
  const app = deviceHarness()
  const firstAuthor = 'd'.repeat(64), otherAuthor = 'e'.repeat(64), otherNetwork = 'f'.repeat(64)
  const peer = { ...entry, device_id: remote }
  const other = { ...entry, device_id: '4'.repeat(64) }
  const verified = app.secure.verifyAuthentication()
  const connect = async (candidate: ConnectionEntry, networkId: string, vector: Record<string, number>) => {
    app.secure.verifyAuthentication = () => ({ ...verified, principal: { ...verified.principal, network_id: networkId }, view: { ...verified.view, network_id: networkId } })
    const info = deferred(), subscription = deferred()
    info.resolve({ device_id: candidate.device_id, name: 'node', version: 'test', initialized: true, network_id: networkId })
    subscription.resolve({ vector })
    app.holds.set('info', info); app.holds.set('subscribe', subscription)
    await app.render().connect(candidate)
    return app.render()
  }
  const subscribedVector = () => plain(app.requests.filter(request => request.method === 'subscribe').at(-1)?.params.vector)
  try {
    app.render(); await settle(); app.render()
    let device = await connect(entry, network, { [firstAuthor]: 10 })
    assert.equal(device.ready?.catchingUp?.(), false)
    assert.deepEqual(subscribedVector(), {})

    device = await connect(peer, network, { [firstAuthor]: 5 })
    assert.equal(device.ready?.catchingUp?.(), true, 'an older entry in the same network must catch up')
    assert.deepEqual(subscribedVector(), { [firstAuthor]: 10 })

    device.disconnect()
    device = await connect(other, otherNetwork, { [otherAuthor]: 2 })
    assert.equal(device.ready?.catchingUp?.(), false, 'another network must not inherit the first network watermark')
    assert.deepEqual(subscribedVector(), {})

    device.disconnect()
    device = await connect(peer, network, { [firstAuthor]: 7 })
    assert.equal(device.ready?.catchingUp?.(), true, 'returning to the first network retains its own watermark')
    assert.deepEqual(subscribedVector(), { [firstAuthor]: 10 })

    device = await connect(entry, network, { [firstAuthor]: 10 })
    assert.equal(device.ready?.catchingUp?.(), false)
  } finally { app.close() }
})

const recoveryCheckpoint = '8'.repeat(64)
function recoveryApprovalHarness() {
  const app = deviceHarness({ management: true, saved: true })
  app.directory.state.checkpoints = [{ epoch: 2, signature: 'ancestor' }, { epoch: 3, signature: 'recovery' }]
  app.directory.view = { ...app.directory.view, epoch: 2, pending: { checkpoint: recoveryCheckpoint, effective_at_ms: app.directory.now_ms + 60000, members: app.directory.view.members } }
  return app
}
function approvedDirectory(app: ReturnType<typeof recoveryApprovalHarness>, change: Partial<MembershipView> = {}) {
  app.directory.state = { ...app.directory.state, events: [{ type: 'approve', checkpoint: recoveryCheckpoint }] }
  app.directory.view = { ...app.directory.view, epoch: 3, pending: null, ...change }
  return app.receipt()
}
async function pendingRecoveryApproval(app: ReturnType<typeof recoveryApprovalHarness>) {
  const reply = deferred()
  app.holds.set('approve_recovery', reply)
  const approval = app.render().approveRecovery(recoveryCheckpoint)
  await until(() => app.requests.some(request => request.method === 'approve_recovery'))
  return { reply, approval }
}

for (const detached of [false, true]) test(`recovery approval verifies its signed receipt${detached ? ' even after this manager is removed' : ''}`, async () => {
  const app = recoveryApprovalHarness()
  try {
    app.render(); await settle(); app.render()
    const { reply, approval } = await pendingRecoveryApproval(app)
    const answer = approvedDirectory(app, detached ? { members: [] } : {})
    if (detached) {
      const unavailable = deferred(); unavailable.promise.catch(() => {}); unavailable.reject(new Error('this manager was removed'))
      app.holds.set('directory', unavailable)
    }
    reply.resolve(answer)
    await approval
    assert.deepEqual(app.signatures, ['identity-governance'])
    assert.deepEqual(plain(app.requests.find(request => request.method === 'approve_recovery')?.params.event), { issuer: subject, action: { type: 'approve', checkpoint: recoveryCheckpoint, at_ms: app.directory.now_ms }, signature: 'signature' })
    if (!detached) {
      await assert.rejects(app.render().approveRecovery(recoveryCheckpoint), /等待确认/)
      assert.equal(app.requests.filter(request => request.method === 'approve_recovery').length, 1)
      assert.equal(app.signatures.length, 1)
    }
  } finally { app.close() }
})

for (const landed of [true, false]) test(`a lost recovery approval reply ${landed ? 'resolves with signed evidence' : 'preserves the request error'} without signing again`, async () => {
  const app = recoveryApprovalHarness()
  try {
    app.render(); await settle(); app.render()
    const { reply, approval } = await pendingRecoveryApproval(app)
    if (landed) approvedDirectory(app)
    const cause = new Error('approval reply lost')
    reply.reject(cause)
    if (landed) await approval
    else await assert.rejects(approval, error => error === cause)
    assert.deepEqual(app.signatures, ['identity-governance'])
    assert.equal(app.requests.filter(request => request.method === 'approve_recovery').length, 1)
  } finally { app.close() }
})

for (const principal of [null, { network_id: network }]) test(`a failed first directory read rejects recovery approval even with principal ${principal === null ? 'null' : 'present'}`, async () => {
  const app = recoveryApprovalHarness()
  try {
    app.render(); await settle(); app.render()
    const authentications = app.calls.filter(call => call === 'authenticate').length
    const read = deferred(), auth = deferred(); auth.resolve({ principal })
    app.holds.set('directory', read); app.holds.set('authenticate', auth)
    const approval = app.render().approveRecovery(recoveryCheckpoint)
    app.holds.delete('directory')
    const cause = new Error('directory unavailable'); read.reject(cause)
    await assert.rejects(approval, error => error === cause)
    assert.deepEqual(app.signatures, [])
    assert.equal(app.requests.some(request => request.method === 'approve_recovery'), false)
    assert.equal(app.calls.filter(call => call === 'authenticate').length, authentications)
  } finally { app.close() }
})

for (const principal of [null, { network_id: network }]) test(`a lost recovery approval reply stays unknown when directory fails and principal is ${principal === null ? 'null' : 'present'}`, async () => {
  const app = recoveryApprovalHarness()
  try {
    app.render(); await settle(); app.render()
    const authentications = app.calls.filter(call => call === 'authenticate').length
    const { reply, approval } = await pendingRecoveryApproval(app)
    const read = deferred(), auth = deferred(); read.promise.catch(() => {}); read.reject(new Error('directory unavailable')); auth.resolve({ principal })
    app.holds.set('directory', read); app.holds.set('authenticate', auth)
    const cause = new Error('approval reply lost'); reply.reject(cause)
    await assert.rejects(approval, error => error === cause)
    assert.deepEqual(app.signatures, ['identity-governance'])
    assert.equal(app.requests.filter(request => request.method === 'approve_recovery').length, 1)
    assert.equal(app.calls.filter(call => call === 'authenticate').length, authentications)
  } finally { app.close() }
})

for (const lost of [false, true]) test(`a ${lost ? 'lost' : 'successful'} recovery approval reply rejects an invalid signed state`, async () => {
  const app = recoveryApprovalHarness()
  try {
    const before = app.directory.state, verify = app.secure.verifyState
    app.secure.verifyState = (state = app.directory.state) => { if (state !== before) throw new Error('invalid signed state'); return verify(state) }
    app.render(); await settle(); app.render()
    const { reply, approval } = await pendingRecoveryApproval(app)
    const answer = approvedDirectory(app), cause = new Error('approval reply lost')
    if (lost) reply.reject(cause)
    else reply.resolve(answer)
    await assert.rejects(approval, error => lost ? error === cause : error instanceof Error && error.message === 'invalid signed state')
    assert.deepEqual(app.signatures, ['identity-governance'])
  } finally { app.close() }
})

for (const status of ['missing', 'different', 'stopped', 'rotated', 'active'] as const) test(`recovery approval rejects a ${status} pending target before signing`, async () => {
  const app = recoveryApprovalHarness()
  try {
    app.render(); await settle(); app.render()
    app.directory.view = { ...app.directory.view, pending: status === 'different' && app.directory.view.pending ? { ...app.directory.view.pending, checkpoint: '9'.repeat(64) } : null }
    if (status === 'stopped' || status === 'rotated') app.directory.view.stopped = [recoveryCheckpoint]
    if (status === 'rotated' || status === 'active') app.directory.view.epoch = 3
    await assert.rejects(app.render().approveRecovery(recoveryCheckpoint), /等待确认/)
    assert.deepEqual(app.signatures, [])
    assert.equal(app.requests.some(request => request.method === 'approve_recovery'), false)
  } finally { app.close() }
})

for (const lost of [false, true]) for (const outcome of ['pending', 'vetoed', 'rotated', 'missing-target', 'missing-ancestor', 'different-target', 'same-epoch', 'unsigned-state', 'unsigned-view', 'invalid-view'] as const) test(`a ${lost ? 'lost' : 'successful'} recovery approval reply cannot confirm ${outcome}`, async () => {
  const app = recoveryApprovalHarness()
  try {
    app.render(); await settle(); app.render()
    const { reply, approval } = await pendingRecoveryApproval(app)
    let answer: unknown = app.receipt()
    if (outcome === 'unsigned-state') answer = { principal: null, view: { ...app.directory.view, epoch: 3, pending: null } }
    else if (outcome === 'unsigned-view') answer = { ...app.receipt(), view: { ...app.directory.view, epoch: 3, pending: null } }
    else if (outcome !== 'pending') {
      const stopped = outcome === 'vetoed' || outcome === 'rotated'
      approvedDirectory(app, { epoch: outcome === 'same-epoch' || outcome === 'vetoed' ? 2 : 3, stopped: stopped ? [recoveryCheckpoint] : [] })
      const chain = app.directory.state.checkpoints
      if (outcome === 'missing-target') app.directory.state.checkpoints = chain.slice(0, -1)
      if (outcome === 'missing-ancestor') app.directory.state.checkpoints = chain.slice(1)
      if (outcome === 'different-target') app.directory.state.checkpoints = [chain[0], { epoch: 3, signature: 'other-recovery' }]
      if (outcome === 'rotated') app.directory.state.checkpoints = [...chain, { epoch: 4, signature: 'rotation' }]
      if (outcome === 'invalid-view') app.directory.view.epoch = 0
      answer = app.receipt()
    }
    const cause = new Error('approval reply lost')
    if (lost) reply.reject(cause)
    else reply.resolve(answer)
    await assert.rejects(approval, error => lost ? error === cause : error instanceof Error)
    assert.deepEqual(app.signatures, ['identity-governance'])
    assert.equal(app.requests.filter(request => request.method === 'approve_recovery').length, 1)
  } finally { app.close() }
})

test('an owner bootstrap link creates the network through recover without HTTP credential issuance', async () => {
  const fragment = `#bootstrap=${Buffer.from(JSON.stringify(localInvitation)).toString('base64url')}`
  const app = deviceHarness({ local: 'fresh', saved: true, fragment })
  app.localState.setup = { kind: 'owner_required', initialized: false }
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup?.kind, 'uninitialized')
    assert.deepEqual(app.calls, ['local_setup'])
    assert.equal(app.setupRequests[0].path, '/api/local/setup')
    assert.equal(app.setupRequests[0].options.method, 'POST')
    assert.equal(app.setupRequests[0].options.body, '{}')
    assert.equal(app.setupRequests[0].options.cache, 'no-store')
    assert.equal(app.setupRequests[0].options.credentials, 'omit')
    assert.equal([...app.stored.values()].some(value => value.includes(localCapability)), false)
    await app.render().createLocalNetwork('browser', 'test-only-passphrase')
    assert.equal(app.render().review?.kind, 'bootstrap')
    assert.equal(app.calls.includes('recover'), false)
    await app.render().downloadBackup()
    await app.render().confirmBootstrap(true)
    assert.deepEqual(plain(app.render().ready?.entry), localEntry)
    assert.equal(app.render().localSetup?.kind, 'initialized')
    const recover = app.requests.filter(request => request.method === 'recover')
    assert.equal(recover.length, 1)
    assert.equal(recover[0].params.capability, localCapability)
    assert.deepEqual(plain(recover[0].params.genesis), genesis)
  } finally { app.close() }
})

test('anonymous localhost status cannot initialize a node or obtain a local launch', async () => {
  for (const fragment of ['', '#bootstrap=not-json', `#bootstrap=${Buffer.from(JSON.stringify({ ...localInvitation, url: 'https://remote.example' })).toString('base64url')}`]) {
    const app = deviceHarness({ local: 'fresh', fragment })
    app.localState.setup = { kind: 'owner_required', initialized: false }
    try {
      app.render(); await settle()
      assert.equal(app.render().localSetup, null)
      assert.equal(app.render().ready, null)
      assert.equal(app.calls.includes('local-exchange'), false)
      assert.equal(app.calls.includes('recover'), false)
      assert.equal(app.navigations.length, 0)
    } finally { app.close() }
  }
})

test('initialized localhost requires member authorization and cannot create a second network', async () => {
  const app = deviceHarness({ local: 'initialized' })
  const hold = deferred(); app.holds.set('authenticate', hold)
  try {
    app.render(); await settle()
    hold.resolve(notMember); await settle()
    assert.equal(app.render().localSetup?.kind, 'initialized')
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.match(app.render().error, /还没加入/)
    await assert.rejects(app.render().createLocalNetwork('browser', 'test-only-passphrase'), /已经设置过/)
    assert.equal(app.calls.includes('recover'), false)
  } finally { app.close() }
})

test('a revoked browser learns it was removed instead of retrying', async () => {
  const app = deviceHarness({ saved: true })
  const hold = deferred(); app.holds.set('authenticate', hold)
  try {
    app.render(); await settle()
    hold.resolve({ ...notMember, revoked: true }); await settle()
    assert.equal(app.render().ready, null)
    assert.match(app.render().error, /已被移除/)
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('initialized localhost authenticates the signed manager on its original entry before offering local access', async () => {
  const app = deviceHarness({ local: 'initialized', management: true })
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'membership')
    assert.equal(app.render().ready?.principal.can_manage, true)
    assert.deepEqual(plain(app.render().ready?.entry), localEntry)
    assert.deepEqual(plain(app.sockets[0]?.entry), localEntry)
    assert.equal(app.calls.includes('local-exchange'), false)
    assert.equal(app.navigations.length, 0)
    assert.equal(app.location.origin, localEntry.address)
  } finally { app.close() }
})

test('opening local access requires a click and fetches a fresh launcher before changing origin', async () => {
  const app = deviceHarness({ local: 'initialized' }), auth = deferred()
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  app.holds.set('authenticate', auth)
  try {
    app.render(); await settle()
    auth.resolve(notMember); await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().localSetup?.kind, 'initialized')
    assert.equal(app.navigations.length, 0)
    assert.equal(app.calls.includes('local-exchange'), false)
    const fresh = { ...localLaunch, token: '1'.repeat(64) }
    app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: fresh }
    await app.render().openLocalAccess()
    assert.equal(app.setupRequests.length, 2)
    assert.equal(app.navigations.length, 1)
    const opened = new URL(app.navigations[0])
    assert.equal(opened.origin, fresh.url)
    assert.equal(new URLSearchParams(opened.hash.slice(1)).get('local'), fresh.token)
    assert.equal(new URLSearchParams(opened.hash.slice(1)).get('node'), target)
    assert.equal([...app.stored.values()].some(value => value.includes(fresh.token)), false)
  } finally { app.close() }
})

test('disabling local automatic access before the click prevents launching its stale token', async () => {
  const app = deviceHarness({ local: 'initialized' }), auth = deferred()
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  app.holds.set('authenticate', auth)
  try {
    app.render(); await settle()
    auth.resolve(notMember); await settle()
    app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry }
    await assert.rejects(app.render().openLocalAccess(), /免确认打开已关闭/)
    assert.equal(app.navigations.length, 0)
    const setup = app.render().localSetup
    assert.equal(setup?.kind === 'initialized' && setup.local_launch, undefined)
  } finally { app.close() }
})

test('a removed computer shows removed on its local page, and rejoining opens it to carry on there', async () => {
  const app = deviceHarness({ local: 'initialized' })
  app.localState.setup = { kind: 'initialized', removed: true, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.render().phase, 'removed')
    assert.equal(app.render().error, '')
    assert.deepEqual(app.calls, ['local_setup'])
    await app.render().rejoin()
    assert.equal(app.navigations.length, 1)
    const opened = new URLSearchParams(new URL(app.navigations[0]).hash.slice(1))
    assert.equal(opened.get('local'), localLaunch.token)
    assert.equal(opened.get('rejoin'), '1')
  } finally { app.close() }
})

test('local launch metadata rejects an unrelated node, unsafe address, or malformed token', async () => {
  const invalid = [
    { ...localLaunch, device_id: '0'.repeat(64) },
    { ...localLaunch, token: 'e'.repeat(63) },
    { ...localLaunch, token: 'E'.repeat(64) },
    ...['https://127.0.0.1:49781', 'http://localhost:49781', 'http://127.0.0.1', 'http://user:password@127.0.0.1:49781', 'http://127.0.0.1:49781/path', 'http://127.0.0.1:49781?token=unsafe', 'http://127.0.0.1:49781#unsafe'].map(url => ({ ...localLaunch, url })),
  ]
  for (const launch of invalid) {
    const app = deviceHarness({ local: 'initialized' })
    app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: launch }
    try {
      app.render(); await settle()
      assert.equal(app.render().localSetup, null)
      assert.match(app.render().error, /本机链接和这个节点对不上/)
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
    await app.render().connect({ ...entry, device_id: target })
    hold.resolve(kind === 'uninitialized' ? { kind, invitation: localInvitation } : { kind, entry: localEntry })
    await settle()
    assert.equal(app.render().localSetup, null)
    assert.equal(app.calls.filter(call => call.startsWith('open:')).length, 1)
    assert.equal(app.calls.includes('recover'), false)
  } finally { app.close() }
})

test('an invitation in the page URL takes precedence over local discovery', async () => {
  const app = deviceHarness({ local: 'fresh', invitation: 'vibeface1ticket' })
  try {
    app.render(); await settle()
    assert.equal(app.render().initialInvitation, 'vibeface1ticket')
    assert.equal(app.render().localSetup, null)
    assert.deepEqual(app.calls, [])
    assert.equal(app.location.hash, '')
  } finally { app.close() }
})

test('a local setup response cannot redirect onboarding to another origin', async () => {
  const app = deviceHarness({ local: 'fresh' })
  const hold = deferred(); app.holds.set('local_setup', hold)
  try {
    app.render(); await settle()
    hold.resolve({ kind: 'uninitialized', invitation: { ...localInvitation, url: entry.address } }); await settle()
    assert.equal(app.render().localSetup, null)
    assert.match(app.render().error, /当前页面对不上/)
    assert.equal(app.sockets.length, 0)
  } finally { app.close() }
})

test('local setup cannot silently select the cookie channel instead of the ordinary member entry', async () => {
  const app = deviceHarness({ local: 'initialized' })
  app.localState.setup = { kind: 'initialized', removed: false, entry: { ...localEntry, local: true }, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup, null)
    assert.match(app.render().error, /本机设置和当前页面对不上/)
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

test('a join asks this page\'s Gateway to forward to the entry Node by its device ID', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.deepEqual(plain(app.sockets.map(socket => socket.entry)), [{ kind: 'gateway', address: entry.address, device_id: target }])
    assert.ok(app.requests.some(request => request.method === 'join'))
  } finally { app.close() }
})

test('the desktop app, whose page is its own bundle, joins through the public node', async () => {
  const publicNode = JSON.parse(readFileSync(new URL('../../../../../config/public-bootstrap.json', import.meta.url), 'utf8')).gateway_url
  const app = deviceHarness({ origin: 'vibeface-app://app' })
  try {
    app.render(); await settle()
    assert.equal(app.render().defaultGateway, publicNode)
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.deepEqual(plain(app.sockets.map(socket => socket.entry)), [{ kind: 'gateway', address: publicNode, device_id: target }])
  } finally { app.close() }
})

test('the official page joins through the public node, or the access address this browser keeps instead', async () => {
  const publicNode = JSON.parse(readFileSync(new URL('../../../../../config/public-bootstrap.json', import.meta.url), 'utf8')).gateway_url
  const storage = new Map<string, string>()
  const app = deviceHarness({ origin: 'https://app.vibeface.dev', storage })
  try {
    app.render(); await settle()
    assert.equal(publicNode, 'https://node.vibeface.dev:4792')
    assert.equal(app.render().gateway, publicNode)
    assert.equal(app.render().defaultGateway, publicNode)
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    app.render().cancelJoin()
    app.render().setGateway('https://mine.example')
    assert.equal(app.render().gateway, 'https://mine.example')
    assert.equal(storage.get('vibeface.gateway'), '"https://mine.example"')
    await app.render().join('vibeface://join/vibeface1other', 'browser')
    assert.deepEqual(plain(app.sockets.map(socket => 'address' in socket.entry ? socket.entry.address : '')).filter((address: string, i: number, all: string[]) => all.indexOf(address) === i), [publicNode, 'https://mine.example'])
    app.render().setGateway(null)
    assert.equal(app.render().gateway, publicNode)
    assert.equal(storage.get('vibeface.gateway'), 'null')
  } finally { app.close() }
})

test('joining commits first, reveals only after the manager nonce, then shows the short code and authenticates', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    const join = app.requests.find(request => request.method === 'join')
    assert.deepEqual(plain(join?.params), { secret: 'one-time-secret', public_key: subject, kind: 'client', name: 'browser', commit: 'd'.repeat(64) })
    assert.equal(app.render().joining?.status, 'pending')
    assert.equal(app.render().joining?.code, undefined)
    assert.equal(app.requests.some(request => request.params.reveal), false)
    assert.equal([...app.stored.values()].some(value => value.includes('one-time-secret')), false)
    const nonce = JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null').nonce
    app.joinStatus.value = joinRequest('confirming', { admin_id: target, admin_nonce: '2'.repeat(64) })
    await app.render().checkJoin()
    assert.deepEqual(app.requests.filter(request => request.params.reveal).map(request => request.params.reveal), [nonce])
    assert.equal(app.render().joining?.code, '123456')
    assert.equal(app.render().ready, null)
    app.joinStatus.value = joinRequest('joined', { admin_id: target, admin_nonce: '2'.repeat(64), joiner_nonce: nonce })
    await app.render().checkJoin()
    assert.equal(app.render().ready?.principal.device_id, subject)
    assert.equal(app.render().joining, null)
    assert.equal(app.stored.get('vibeface.pending-join'), 'null')
    assert.equal(app.requests.filter(request => request.method === 'join').length, 1)
    assert.equal(app.calls.includes('replace-key'), false)
  } finally { app.close() }
})

test('opening a used link says so before 加入, on its own channel, and sends no join request', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    const phase = app.render().phase
    app.inviteState.value = 'used'
    assert.equal(await app.render().checkInvite('vibeface://join/vibeface1ticket'), 'used')
    assert.deepEqual(plain(app.requests.filter(request => request.method === 'join_check').map(request => request.params)), [{ secret: 'one-time-secret' }])
    assert.equal(app.requests.some(request => request.method === 'join'), false, 'checking takes nothing')
    assert.equal(app.sockets.length, 1)
    assert.equal(app.render().phase, phase, 'the screen keeps its connection state')
    app.inviteState.value = 'open'
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.equal(app.render().joining?.status, 'pending', 'an open link still goes through the approval')
  } finally { app.close() }
})

test('a link whose entry Node listens at addresses is checked and joined straight there first', async () => {
  const app = deviceHarness({ entryAddrs: ['192.168.1.4:7001'] })
  try {
    app.render(); await settle()
    const before = app.sockets.length
    assert.equal(await app.render().checkInvite('vibeface://join/vibeface1ticket'), 'open')
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.deepEqual(plain(app.sockets.slice(before).map(socket => socket.entry)), [
      { kind: 'lan', addrs: ['192.168.1.4:7001'], device_id: target },
      { kind: 'lan', addrs: ['192.168.1.4:7001'], device_id: target },
    ])
    assert.equal(app.render().joining?.status, 'pending')
  } finally { app.close() }
})

test('this computer asks its own Node about a link, with the page capability, and never joins to find out', async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await until(() => app.render().localSetup?.kind === 'uninitialized')
    app.inviteState.value = 'expired'
    assert.equal(await app.render().checkInvite('vibeface://join/vibeface1ticket'), 'expired')
    const check = app.requests.find(request => request.method === 'node_join_check')
    assert.deepEqual(plain(check?.params), { capability: localCapability, link: 'vibeface://join/vibeface1ticket' })
    assert.equal(app.requests.some(request => request.method === 'node_join'), false)
  } finally { app.close() }
})

test('this computer\'s join asks again after a lost answer and explains a failure as the terminal does', async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await until(() => app.render().localSetup?.kind === 'uninitialized')
    const lost = deferred(); app.holds.set('node_join', lost)
    const joining = app.render().joinThisComputer('vibeface://join/vibeface1ticket', '工作室 Mac')
    await until(() => app.requests.some(request => request.method === 'node_join'))
    const before = new Set(app.timers.keys())
    app.holds.set('node_join', { promise: Promise.resolve({ status: 'failed', reason: 'join request Rejected: rejected' }), resolve() {}, reject() {} })
    lost.reject(new Error('request timed out'))
    await until(() => [...app.timers.keys()].some(id => !before.has(id)))
    for (const [id, run] of app.timers) if (!before.has(id)) { app.timers.delete(id); run() }
    await assert.rejects(joining, /管理设备拒绝了这次加入/)
    const asked = app.requests.filter(request => request.method === 'node_join').map(request => plain(request.params))
    assert.equal(asked.length, 2)
    assert.deepEqual(asked[1], asked[0], 'the same link again only reads how far the join got')
  } finally { app.close() }
})

for (const landed of [true, false]) test(`a lost rename reply reads the name back ${landed ? 'and counts it' : 'and reports the failure'}, never signing twice`, async () => {
  const app = deviceHarness({ management: true, saved: true })
  try {
    app.render(); await settle(); app.render()
    await app.render().refreshDirectory()
    let name = 'node'
    const verify = app.secure.verifyState
    app.secure.verifyState = () => ({ ...verify(), viewJson: () => JSON.stringify({ ...membership, members: [{ device_id: subject, kind: 'client', name: 'browser', manage: true }, { device_id: target, kind: 'node', name, manage: false }] }) })
    const reply = deferred(); app.holds.set('grant', reply)
    const renaming = app.render().renameDevice(target, ' 工作室 Mac ')
    await until(() => app.requests.some(request => request.method === 'grant'))
    if (landed) name = '工作室 Mac'
    reply.reject(new Error('connection lost'))
    if (landed) await renaming
    else await assert.rejects(renaming, /connection lost/)
    const grants = app.requests.filter(request => request.method === 'grant').map(request => plain(request.params.event))
    assert.equal(grants.length, 1)
    assert.deepEqual(grants[0].action, { type: 'grant', member: { device_id: target, kind: 'node', name: '工作室 Mac', manage: false } }, 'a rename keeps the kind and permission')
    if (landed) {
      await app.render().renameDevice(target, '工作室 Mac')
      assert.equal(app.requests.filter(request => request.method === 'grant').length, 1, 'a name already in place is not signed again')
    }
  } finally { app.close() }
})

test('a removed browser joins again as a new device and never offers its revoked key', async () => {
  const app = deviceHarness({ removed: true })
  try {
    app.render(); await settle()
    app.joinStatus.value = joinRequest('pending', { device_id: freshKey })
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.deepEqual(app.requests.filter(request => request.method === 'join').map(request => request.params.public_key), [freshKey])
    assert.equal(app.render().identity?.deviceId, freshKey)
    assert.equal(app.render().joining?.status, 'pending')
  } finally { app.close() }
})

for (const [status, reason, message] of [['rejected', 'commitment_mismatch', /校验失败/], ['rejected', 'rejected', /拒绝/], ['expired', null, /过期/]] as const) test(`a ${reason ?? status} join is cleared and explained`, async () => {
  const app = deviceHarness({ joining: 'pending' })
  try {
    app.render(); await settle()
    app.joinStatus.value = joinRequest(status, { reason })
    await assert.rejects(app.render().checkJoin(), message)
    assert.equal(app.render().joining, null)
    assert.equal(app.stored.get('vibeface.pending-join'), 'null')
  } finally { app.close() }
})

test('a join link opened in the same tab after a rejection replaces the spent one and its error', async () => {
  const app = deviceHarness({ joining: 'pending' })
  try {
    app.render(); await settle()
    app.joinStatus.value = joinRequest('rejected', { reason: 'rejected' })
    await assert.rejects(app.render().checkJoin(), /拒绝/)
    assert.match(app.render().error, /拒绝/)
    app.openHash('/join', '#local=token')
    assert.equal(app.location.hash, '#local=token')
    app.openHash('/join', '#vibeface1fresh')
    assert.equal(app.render().initialInvitation, 'vibeface1fresh')
    assert.equal(app.render().error, '')
    assert.equal(app.location.hash, '')
  } finally { app.close() }
})

test('a pending join survives reload without resubmitting', async () => {
  const app = deviceHarness({ joining: 'pending' }), status = deferred()
  app.holds.set('join_status', status)
  try {
    app.render(); await until(() => app.calls.includes('join_status'))
    assert.equal(app.render().joining?.status, 'checking')
    status.resolve(joinRequest('pending'))
    app.render(); await settle()
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'join_status'])
    assert.equal(app.render().joining?.join_id, inviteId)
    assert.equal(app.render().joining?.status, 'pending')
  } finally { app.close() }
})

test('a pending join reaches its original Node before remembered localhost access', async () => {
  const app = deviceHarness({ joining: 'pending', localSession: true })
  const saved = app.stored.get('vibeface.pending-join')
  try {
    app.render(); await settle()
    assert.equal(app.render().joining?.nonce, joinNonce)
    assert.equal(app.render().ready, null)
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'join_status'])
    assert.deepEqual(plain(app.sockets[0]?.entry), entry)
    assert.equal(app.stored.get('vibeface.pending-join'), saved)
    app.render().cancelJoin()
    assert.equal(app.stored.get('vibeface.pending-join'), 'null')
    assert.equal(app.calls.filter(call => call === 'join_withdraw').length, 1)
  } finally { app.close() }
})

test('a new invitation cannot replace the nonce of a saved pending join', async () => {
  const app = deviceHarness({ joining: 'pending', invitation: 'vibeface1newticket' })
  try {
    app.render(); await settle()
    const original = app.stored.get('vibeface.pending-join')
    assert.equal(app.render().joining?.nonce, joinNonce)
    await assert.rejects(app.render().join('vibeface://join/vibeface1newticket', 'browser'), /本地加入记录/)
    assert.equal(app.requests.some(request => request.method === 'join'), false)
    assert.equal(app.stored.get('vibeface.pending-join'), original)
  } finally { app.close() }
})

test('damaged pending join storage is preserved until explicit cancellation', async () => {
  const original = '{', storage = new Map([['vibeface.pending-join', original]])
  const app = deviceHarness({ storage })
  try {
    app.render(); await settle()
    assert.equal(app.render().joining, null)
    assert.equal(app.calls.includes('join_status'), false)
    await assert.rejects(app.render().join('vibeface://join/vibeface1ticket', 'browser'), /本地加入记录/)
    assert.equal(app.stored.get('vibeface.pending-join'), original)
    assert.equal(app.calls.includes('join'), false)
    app.render().cancelJoin()
    assert.equal(app.stored.get('vibeface.pending-join'), 'null')
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.equal(app.calls.filter(call => call === 'join').length, 1)
  } finally { app.close() }
})

test('first join requires the exact local write to succeed and be readable before it sends', async () => {
  for (const fail of ['throw', 'lose']) {
    const app = deviceHarness()
    try {
      app.render(); await settle()
      const write = app.platform.storage.setItem
      app.platform.storage.setItem = (key: string, value: string) => {
        if (key !== 'vibeface.pending-join') return write(key, value)
        if (fail === 'throw') throw new Error('local write refused')
      }
      await assert.rejects(app.render().join('vibeface://join/vibeface1ticket', 'browser'), /无法保存设备记录|本地加入记录/)
      assert.equal(app.calls.includes('join'), false)
      assert.equal(app.render().joining, null)
    } finally { app.close() }
  }
})

test('a lost first join receipt reopens the original Node only to check the durably saved intent', async () => {
  const app = deviceHarness(), send = deferred()
  app.holds.set('join', send)
  try {
    app.render(); await settle()
    const joining = app.render().join('vibeface://join/vibeface1ticket', 'browser')
    const lost = assert.rejects(joining, /connection lost/)
    await until(() => app.calls.includes('join'))
    assert.equal(app.render().joining?.status, 'checking')
    const original = app.stored.get('vibeface.pending-join')
    assert.ok(original)
    assert.equal(original.includes('one-time-secret'), false)
    const nonce = JSON.parse(original).nonce
    await app.render().checkJoin()
    assert.equal(app.calls.includes('join_status'), false)
    app.sockets.at(-1)?.drop(); send.reject(new Error('connection lost'))
    await lost
    const missing = deferred(); app.holds.set('join_status', missing)
    const checking = assert.rejects(app.render().checkJoin(), /join request not found/)
    await until(() => app.calls.includes('join_status'))
    missing.reject(new Error('join request not found'))
    await checking
    assert.equal(app.render().joining?.status, 'checking')
    assert.equal(app.stored.get('vibeface.pending-join'), original)
    app.holds.delete('join_status')
    await app.render().checkJoin()
    assert.equal(app.render().joining?.status, 'pending')
    assert.equal(app.render().error, '', 'a successful status check clears the connection error')
    assert.equal(app.render().joining?.nonce, nonce)
    assert.deepEqual(app.sockets.map(socket => socket.entry.device_id), [target, target])
    assert.equal(app.requests.filter(request => request.method === 'join').length, 1)
    assert.ok(app.requests.filter(request => request.method === 'join_status').every(request => request.params.join_id === inviteId && !('reveal' in request.params)))
  } finally { app.close() }
})

for (const refusal of ['ticket has expired', 'ticket has already been used']) test(`a confirmed join refusal clears the saved intent when ${refusal}`, async () => {
  const app = deviceHarness(), send = deferred()
  app.holds.set('join', send)
  try {
    app.render(); await settle()
    const joining = assert.rejects(app.render().join('vibeface://join/vibeface1ticket', 'browser'), { message: spentInviteText })
    await until(() => app.calls.includes('join'))
    assert.equal(app.render().joining?.status, 'checking')
    send.reject(Object.assign(new Error(refusal), { responded: true }))
    await joining
    assert.equal(app.render().joining, null)
    assert.equal(app.render().error, spentInviteText)
    assert.equal(app.stored.get('vibeface.pending-join'), 'null')
    assert.equal(app.calls.includes('join_status'), false)
  } finally { app.close() }
})

test('a mismatched Node request is refused before pinning or revealing a saved join nonce', async () => {
  const app = deviceHarness({ joining: 'pending' })
  try {
    app.render(); await settle()
    const original = app.stored.get('vibeface.pending-join')
    const changed = joinRequest('confirming', { commit: 'e'.repeat(64) })
    app.joinStatus.value = changed
    await assert.rejects(app.render().checkJoin(), /加入状态.*不一致/)
    assert.equal(app.requests.some(request => request.params.reveal), false)
    assert.equal(app.stored.get('vibeface.pending-join'), original)
  } finally { app.close() }
})

test('a joiner keeps the manager nonce it saw before revealing and stops when the entry changes it', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    const nonce = JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null').nonce
    app.joinStatus.value = joinRequest('confirming', { admin_id: target, admin_nonce: '2'.repeat(64) })
    await app.render().checkJoin()
    assert.equal(app.render().joining?.code, '123456')
    assert.equal(JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null').pinned.admin_nonce, '2'.repeat(64))
    app.joinStatus.value = joinRequest('confirming', { admin_id: target, admin_nonce: '3'.repeat(64), joiner_nonce: nonce })
    await assert.rejects(app.render().checkJoin(), /改动了加入请求/)
    assert.equal(app.render().joining?.nonce, nonce)
    assert.equal(app.render().joining?.code, undefined)
    assert.equal(JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null').pinned.admin_nonce, '2'.repeat(64))
  } finally { app.close() }
})

test('a manager shows the short code only for its own nonce and the commitment it saw before sending it', async () => {
  const app = deviceHarness({ management: true, saved: true })
  try {
    app.render(); await settle(); app.render()
    app.managed.value = [joinRequest('pending', { device_id: remote })]
    await app.render().refreshDirectory(); app.render()
    await app.render().startApproval(inviteId)
    const sent = app.requests.filter(request => request.method === 'approve').map(request => request.params.admin_nonce)
    const revealed = { device_id: remote, admin_id: subject, admin_nonce: sent[0], joiner_nonce: joinNonce }
    for (const [change, code] of [[{}, '123456'], [{ commit: 'e'.repeat(64) }, undefined], [{ admin_nonce: '3'.repeat(64) }, undefined], [{ device_id: '7'.repeat(64) }, undefined]] as const) {
      app.managed.value = [joinRequest('confirming', { ...revealed, ...change })]
      await app.render().refreshDirectory()
      assert.equal(app.render().joins[0]?.code, code)
    }
    await assert.rejects(app.render().startApproval(inviteId), /审批记录/)
    app.managed.value = [joinRequest('confirming', revealed)]
    await app.render().refreshDirectory(); app.render()
    await app.render().startApproval(inviteId)
    assert.deepEqual(app.requests.filter(request => request.method === 'approve').map(request => request.params.admin_nonce), sent)
  } finally { app.close() }
})

test('a generated link carries the decoded invitation ID so the UI can identify its request', async () => {
  const app = deviceHarness({ management: true, saved: true })
  try {
    app.render(); await settle()
    const invitation = await app.render().createInvitation()
    assert.equal(invitation.inviteId, inviteId)
    assert.equal(invitation.link, 'vibeface://join/vibeface1test')
    assert.ok(invitation.expiresAt > Date.now())
  } finally { app.close() }
})

for (const received of [false, true]) test(`a lost approval response is checked before restarting when the node ${received ? 'received' : 'missed'} the write`, async () => {
  const app = deviceHarness({ management: true, saved: true }), response = deferred()
  app.holds.set('approve', response)
  try {
    app.render(); await settle()
    app.managed.value = [joinRequest('pending', { device_id: remote })]
    await app.render().refreshDirectory()
    const first = assert.rejects(app.render().startApproval(inviteId), /connection lost/)
    await until(() => app.calls.includes('approve'))
    await assert.rejects(app.render().startApproval(inviteId), /正在为这条申请/)
    const nonce = app.requests.find(request => request.method === 'approve')?.params.admin_nonce
    response.reject(new Error('connection lost'))
    await first
    app.holds.delete('approve')
    if (received) app.managed.value = [joinRequest('confirming', { device_id: remote, admin_id: subject, admin_nonce: nonce, joiner_nonce: joinNonce })]
    const before = app.calls.length
    await app.render().startApproval(inviteId)
    assert.equal(app.calls[before], 'join_status')
    assert.deepEqual(app.requests.filter(request => request.method === 'approve').map(request => request.params.admin_nonce), received ? [nonce] : [nonce, nonce])
    if (received) assert.equal(app.render().joins[0]?.code, '123456')
  } finally { app.close() }
})

test('a damaged approval map keeps an existing bound nonce usable without rewriting the map', async () => {
  const newJoinId = '2'.repeat(32), damagedJoinId = '3'.repeat(32)
  const savedPin = { invite_id: inviteId, device_id: remote, nonce: '4'.repeat(64), commit: 'd'.repeat(64) }
  const original = JSON.stringify({ [inviteId]: savedPin, [damagedJoinId]: { nonce: false } })
  const app = deviceHarness({ management: true, saved: true, storage: new Map([['vibeface.approval-pins', original]]) })
  try {
    app.render(); await settle(); app.render()
    app.managed.value = [joinRequest('pending', { device_id: remote }), joinRequest('pending', { join_id: newJoinId, invite_id: newJoinId })]
    await app.render().refreshDirectory()
    await app.render().startApproval(inviteId)
    await assert.rejects(app.render().startApproval(newJoinId), /本地审批记录/)
    assert.deepEqual(app.requests.filter(request => request.method === 'approve').map(request => request.params.admin_nonce), [savedPin.nonce])
    assert.equal(app.stored.get('vibeface.approval-pins'), original)
  } finally { app.close() }
})

test('a join receipt for another invite or device is refused', async () => {
  for (const mismatch of [{ invite_id: '2'.repeat(32) }, { device_id: '5'.repeat(64) }]) {
    const app = deviceHarness()
    try {
      app.render(); await settle()
      app.holds.set('join', { promise: Promise.resolve(joinRequest('pending', mismatch)), resolve() {}, reject() {} })
      await assert.rejects(app.render().join('vibeface://join/vibeface1ticket', 'browser'), /对不上/)
      const pending = JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null')
      assert.equal(pending.join_id, inviteId)
      assert.equal(app.render().joining?.nonce, pending.nonce)
    } finally { app.close() }
  }
})

test('recovering onto a fresh localhost Node uses its local capability and keeps every member by default', async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await settle()
    await app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: localEntry })
    const review = app.render().review
    assert.equal(review?.kind, 'recovery')
    assert.equal(app.calls.includes('recovery_state'), false)
    await app.render().confirmRecovery(review?.kind === 'recovery' ? review.members : [])
    const recover = app.requests.find(request => request.method === 'recover')
    assert.equal(recover?.params.capability, localCapability)
    assert.deepEqual(plain(recover?.params.checkpoints), review?.kind === 'recovery' ? plain([recoveryCheckpointFixture([...review.members, review.newManager])]) : [])
    assert.equal(app.released(), 2, 'the recovery key and the one-time next key are both freed')
  } finally { app.close() }
})

test('recovering onto a remote fresh Node needs that Node\'s own page', async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await settle()
    await assert.rejects(app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: { ...entry, device_id: remote } }), /本机页面/)
    assert.equal(app.calls.includes('recover'), false)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

test('a recovery link naming a computer outside the network is refused before anything is signed', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await assert.rejects(app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry: { ...entry, device_id: remote } }), /不在你的网络里/)
    assert.equal(app.render().review, null)
    assert.equal(app.calls.includes('recover'), false)
  } finally { app.close() }
})

test('recovering an initialized network reads its state with a short-lived root proof', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
    const read = app.requests.find(request => request.method === 'recovery_state')
    assert.deepEqual(plain(read?.params.checkpoints), [])
    assert.equal(read?.params.signature, 'signature')
    assert.ok(Number(read?.params.expires_at_ms) > Date.now())
    const review = app.render().review
    assert.equal(review?.kind === 'recovery' && review.members.some(member => member.device_id === target), true)
    await app.render().confirmRecovery(review?.kind === 'recovery' ? review.members : [])
    assert.equal(app.requests.find(request => request.method === 'recover')?.params.capability, undefined)
  } finally { app.close() }
})

test('a recovery that has to wait is remembered across a reload until a device stops it', async () => {
  const storage = new Map<string, string>(), app = deviceHarness({ storage })
  let checkpoint_hash = ''
  try {
    app.render(); await settle()
    await app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
    const review = app.render().review, answer = deferred()
    app.holds.set('recover', answer)
    const confirming = app.render().confirmRecovery(review?.kind === 'recovery' ? review.members : [])
    await until(() => app.calls.includes('recover'))
    checkpoint_hash = checkpointHash((app.requests.find(request => request.method === 'recover')?.params.checkpoints as unknown[])[0])
    answer.resolve({ ...notMember, recovery: { status: 'waiting', checkpoint_hash, effective_at_ms: 1000 + 48 * 3_600_000 }, now_ms: 1000 })
    await confirming
    const waiting = app.render().recovery
    assert.ok(waiting?.status === 'waiting' && waiting.effectiveAt !== null && waiting.effectiveAt > Date.now() + 47 * 3_600_000, 'the wait is counted on the Node\'s clock')
    assert.equal(app.render().ready, null)
  } finally { app.close() }
  const reloaded = deviceHarness({ storage }), stopped = deferred()
  stopped.resolve({ ...notMember, recovery: { status: 'stopped', checkpoint_hash }, now_ms: Date.now() })
  reloaded.holds.set('authenticate', stopped)
  try {
    reloaded.render()
    await until(() => reloaded.render().recovery?.status === 'stopped')
    reloaded.render().dismissRecovery()
    assert.equal(reloaded.render().recovery, null)
    assert.equal(storage.get('vibeface.recovery-wait'), 'null')
  } finally { reloaded.close() }
})

test('the same file brings a browser that lost its wait record back to the wait', async () => {
  const storage = new Map<string, string>(), app = deviceHarness({ storage }), refused = deferred(), waiting = deferred()
  refused.promise.catch(() => {}); refused.reject(new Error('another recovery is waiting to take effect'))
  waiting.resolve({ ...notMember, recovery: { status: 'waiting', checkpoint_hash: '8'.repeat(64), effective_at_ms: 1000 + 48 * 3_600_000 }, now_ms: 1000 })
  app.holds.set('recovery_state', refused); app.holds.set('authenticate', waiting)
  try {
    app.render(); await settle()
    await app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
    assert.equal(app.render().review, null)
    assert.equal(app.render().recovery?.status, 'waiting')
    assert.equal(app.released(), 1, 'the file is closed again')
    const wait = JSON.parse(storage.get('vibeface.recovery-wait') ?? 'null')
    assert.equal(wait?.rootKey, network)
    assert.equal(app.calls.includes('recover'), false, 'nothing is sent again')
  } finally { app.close() }
})

test('a file whose waiting recovery does not name this browser still reports the wait', async () => {
  const app = deviceHarness({}), refused = deferred(), other = deferred()
  refused.promise.catch(() => {}); refused.reject(new Error('another recovery is waiting to take effect'))
  other.resolve({ ...notMember, recovery: null, now_ms: 1000 })
  app.holds.set('recovery_state', refused); app.holds.set('authenticate', other)
  try {
    app.render(); await settle()
    await assert.rejects(app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry }), /another recovery is waiting/)
    assert.equal(app.render().recovery, null)
  } finally { app.close() }
})

test('a damaged recovery wait does not reconnect or sign and remains stored until explicit dismissal', async () => {
  const original = JSON.stringify({ entry: { address: entry.address, device_id: target }, networkId: network, rootKey: network, genesis, checkpoint: [] })
  const app = deviceHarness({ storage: new Map([['vibeface.recovery-wait', original]]) })
  try {
    app.render(); await settle()
    assert.equal(app.render().recovery, null)
    assert.equal(app.calls.some(call => call.startsWith('open:')), false)
    await assert.rejects(app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry }), /本地找回记录/)
    assert.equal(app.calls.includes('recover'), false)
    assert.equal(app.stored.get('vibeface.recovery-wait'), original)
    app.render().dismissRecovery()
    assert.equal(app.stored.get('vibeface.recovery-wait'), 'null')
  } finally { app.close() }
})

test('a missing or unrelated recovery receipt keeps the exact submitted checkpoint', async () => {
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const original = JSON.stringify({ entry: { address: entry.address, device_id: target }, networkId: network, rootKey: network, genesis, checkpoint })
  for (const recovery of [null, { status: 'stopped', checkpoint_hash: '1'.repeat(64) }]) {
    const app = deviceHarness({ storage: new Map([['vibeface.recovery-wait', original]]) })
    const guest = deferred()
    guest.resolve({ ...notMember, recovery })
    app.holds.set('authenticate', guest)
    try {
      app.render()
      await until(() => /本地找回记录/.test(app.render().error))
      assert.notEqual(app.render().recovery?.status, 'stopped')
      assert.equal(app.stored.get('vibeface.recovery-wait'), original)
      const stopped = deferred(); stopped.resolve({ ...notMember, recovery: { status: 'stopped', checkpoint_hash: checkpointHash(checkpoint) } })
      app.holds.set('authenticate', stopped)
      for (const tick of app.timers.values()) tick()
      await until(() => app.render().recovery?.status === 'stopped')
      assert.equal(app.stored.get('vibeface.recovery-wait'), original)
      assert.equal(app.calls.includes('recover'), false)
    } finally { app.close() }
  }
})

test('two confirmations share one recovery plan and a dropped submission only reads its original receipt', async () => {
  const app = deviceHarness(), saving = deferred(), submitting = deferred(), guest = deferred()
  const input = () => ({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
  const copies: Uint8Array[] = []
  app.platform.saveRecovery = async bytes => { copies.push(bytes.slice()); await saving.promise }
  app.holds.set('recover', submitting)
  guest.resolve({ ...notMember, recovery: null }); app.holds.set('authenticate', guest)
  try {
    app.render(); await settle()
    await app.render().beginRecovery(input())
    const review = app.render().review
    assert.ok(review?.kind === 'recovery')
    const first = assert.rejects(app.render().confirmRecovery(review.members), /正在向原设备查询进度/)
    await until(() => copies.length === 1)
    const second = assert.rejects(app.render().confirmRecovery([]), /正在向原设备查询进度/)
    await settle()
    assert.equal(copies.length, 1)
    assert.equal(app.calls.includes('recover'), false)
    saving.resolve(null)
    await until(() => app.calls.includes('recover'))
    const original = app.stored.get('vibeface.recovery-wait')
    const backup = app.render().unsavedRecovery?.code
    assert.ok(original && backup)
    app.sockets.at(-1)?.drop(); submitting.reject(new Error('connection lost'))
    await Promise.all([first, second])
    assert.equal(app.calls.filter(call => call === 'authenticate').length, 1)
    assert.equal(app.render().recovery?.status, 'waiting')
    const stopped = deferred(); stopped.resolve({ ...notMember, recovery: { status: 'stopped', checkpoint_hash: checkpointHash(JSON.parse(original).checkpoint) } })
    app.holds.set('authenticate', stopped)
    await app.render().beginRecovery(input())
    assert.equal(app.render().recovery?.status, 'stopped')
    assert.equal(app.calls.filter(call => call === 'recover').length, 1)
    assert.equal(app.calls.filter(call => call === 'recovery_state').length, 1)
    assert.equal(app.stored.get('vibeface.recovery-wait'), original)
    assert.equal(app.render().unsavedRecovery?.code, backup)
    assert.equal(copies.length, 1)
  } finally { app.close() }
})

test('a failed authentication on the re-opened recovery channel leaves polling re-entrant', async () => {
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const original = JSON.stringify({ entry: { address: entry.address, device_id: target }, networkId: network, rootKey: network, genesis, checkpoint })
  const app = deviceHarness({ storage: new Map([['vibeface.recovery-wait', original]]) })
  const probe = deferred(), retry = deferred()
  app.holds.set('authenticate', probe)
  try {
    app.render(); await until(() => app.calls.includes('authenticate'))
    app.holds.set('authenticate', retry)
    probe.resolve({ ...notMember, principal: { device_id: subject } })
    await until(() => app.calls.filter(call => call === 'authenticate').length === 2)
    retry.reject(new Error('入口暂时无法认证。'))
    await until(() => /入口暂时无法认证/.test(app.render().error))
    assert.equal(app.render().ready, null)
    assert.equal(app.stored.get('vibeface.recovery-wait'), original)
    const stopped = deferred(); stopped.resolve({ ...notMember, recovery: { status: 'stopped', checkpoint_hash: checkpointHash(checkpoint) } })
    app.holds.set('authenticate', stopped)
    for (const tick of app.timers.values()) tick()
    await until(() => app.render().recovery?.status === 'stopped')
    assert.equal(app.calls.includes('recover'), false)
  } finally { app.close() }
})

test('changing Node while preparing the recovery copy prevents submission on either connection', async () => {
  const app = deviceHarness(), saving = deferred()
  let savedCopy = false
  app.platform.saveRecovery = async () => { savedCopy = true; await saving.promise }
  try {
    app.render(); await settle()
    await app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
    const review = app.render().review
    assert.ok(review?.kind === 'recovery')
    const cancelled = assert.rejects(app.render().confirmRecovery(review.members), /连接已切换/)
    await until(() => savedCopy)
    await app.render().connect({ ...entry, device_id: remote })
    saving.resolve(null)
    await cancelled
    assert.equal(app.render().ready?.entry.device_id, remote)
    assert.equal(app.calls.includes('recover'), false)
    assert.equal(app.stored.has('vibeface.recovery-wait'), false)
  } finally { app.close() }
})

test('a direct recovery response without a receipt closes the guest channel and waits for a fresh read', async () => {
  const app = deviceHarness(), submitted = deferred(), poll = deferred()
  submitted.resolve({ ...notMember, recovery: null }); app.holds.set('recover', submitted)
  app.holds.set('authenticate', poll)
  try {
    app.render(); await settle()
    await app.render().beginRecovery({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
    const review = app.render().review
    assert.ok(review?.kind === 'recovery')
    await app.render().confirmRecovery(review.members)
    assert.equal(app.render().recovery?.status, 'waiting')
    const original = app.stored.get('vibeface.recovery-wait')
    assert.ok(original)
    for (const tick of app.timers.values()) tick()
    await until(() => app.calls.includes('authenticate'))
    app.render().disconnect()
    poll.resolve({ ...notMember, recovery: { status: 'stopped', checkpoint_hash: checkpointHash(JSON.parse(original).checkpoint) } })
    await settle(); await settle()
    assert.equal(app.render().recovery, null)
    assert.equal(app.stored.get('vibeface.recovery-wait'), original)
    assert.equal(app.calls.filter(call => call === 'recover').length, 1)
  } finally { app.close() }
})

test('an authenticated recovery retries a transient wait-clear failure without another Node request', async () => {
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const wait = JSON.stringify({ entry: { address: entry.address, device_id: target }, networkId: network, rootKey: network, genesis, checkpoint })
  const app = deviceHarness({ storage: new Map([['vibeface.recovery-wait', wait]]) })
  const accepted = deferred(); accepted.resolve({ principal: { device_id: subject } }); app.holds.set('authenticate', accepted)
  const verifyAuthentication = app.secure.verifyAuthentication
  app.secure.verifyAuthentication = () => {
    const verified = verifyAuthentication()
    return { ...verified, view: { ...verified.view, recovery_key: nextRecoveryKey } }
  }
  let clears = 0
  const write = app.platform.storage.setItem
  app.platform.storage.setItem = (key: string, value: string) => {
    if (key === 'vibeface.recovery-wait' && value === 'null' && ++clears === 1) throw new Error('local store temporarily unavailable')
    write(key, value)
  }
  try {
    app.render(); await until(() => clears === 1 && app.render().ready !== null)
    const ready = app.render().ready
    assert.equal(app.render().recovered, false)
    assert.equal(app.render().recovery?.status, 'waiting')
    assert.equal(app.stored.get('vibeface.recovery-wait'), wait)
    assert.equal(app.stored.get('vibeface.recovered'), 'true')
    for (const tick of app.timers.values()) tick()
    assert.equal(clears, 2)
    assert.equal(app.stored.get('vibeface.recovery-wait'), 'null')
    assert.equal(app.render().ready, ready)
    assert.equal(app.render().recovery, null)
    assert.equal(app.render().recovered, true)
    assert.equal(app.calls.filter(call => call === 'authenticate').length, 2)
    assert.equal(app.calls.includes('recover'), false)
  } finally { app.close() }
})

for (const reconnect of ['original checkpoint', 'different checkpoint', 'different Node'] as const) test(`recovery cleanup after automatic reconnect with ${reconnect}`, async () => {
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const wait = JSON.stringify({ entry: { address: entry.address, device_id: target }, networkId: network, rootKey: network, genesis, checkpoint })
  const savedCopy = JSON.stringify({ name: 'vibeface-recovery-ABCDEF12.json', code: 'ABCDEF12', attempted: true, data: 'AQID' })
  const tabStorage = new Map([['vibeface.unsaved-recovery', savedCopy]])
  const app = deviceHarness({ storage: new Map([['vibeface.recovery-wait', wait]]), tabStorage })
  const accepted = deferred(); accepted.resolve({ principal: { device_id: subject } }); app.holds.set('authenticate', accepted)
  const verifyAuthentication = app.secure.verifyAuthentication
  let checkpoints = [checkpoint], recoveryKey = nextRecoveryKey
  app.secure.verifyAuthentication = () => {
    const verified = verifyAuthentication()
    const auth = { ...verified.auth, state: { genesis, checkpoints, events: [] } }
    return { ...verified, auth, view: { ...verified.view, recovery_key: recoveryKey } }
  }
  let clears = 0
  const write = app.platform.storage.setItem
  app.platform.storage.setItem = (key: string, value: string) => {
    if (key === 'vibeface.recovery-wait' && value === 'null' && ++clears === 1) throw new Error('local store temporarily unavailable')
    write(key, value)
  }
  try {
    app.render(); await until(() => clears === 1 && app.render().ready !== null)
    const first = app.render().ready
    const timers = new Set(app.timers.keys())
    if (reconnect === 'different checkpoint') checkpoints = [{ ...checkpoint, signature: '8'.repeat(128) }]
    if (reconnect === 'original checkpoint') recoveryKey = freshKey // A later key rotation does not replace the already verified recovery result.
    if (reconnect === 'different Node') {
      app.stored.set(`vibeface.entrances.${network}`, JSON.stringify([{ device_id: remote, gateway_url: 'https://other.example', public: true, seq: 1, direct: [], last_ok_ms: 0 }]))
      const down = deferred(); down.promise.catch(() => {}); down.reject(new Error('connection refused'))
      app.holds.set(`open:${entry.address}`, down)
    }
    app.sockets.at(-1)?.drop()
    for (const [id, retry] of app.timers) if (!timers.has(id)) { app.timers.delete(id); retry(); break }
    await until(() => app.render().ready?.isCurrent() === true && app.render().ready !== first)
    for (const tick of app.timers.values()) tick()
    const complete = reconnect === 'original checkpoint'
    assert.equal(app.render().ready?.entry.device_id, reconnect === 'different Node' ? remote : target)
    assert.equal(app.render().recovered, complete)
    assert.equal(app.stored.get('vibeface.recovery-wait'), complete ? 'null' : wait)
    assert.equal(clears, complete ? 2 : 1)
    assert.equal(tabStorage.get('vibeface.unsaved-recovery'), savedCopy)
    assert.equal(app.calls.filter(call => call === 'authenticate').length, 3)
    assert.equal(app.calls.includes('recover'), false)
    if (reconnect === 'different Node') {
      assert.equal(app.stored.get('vibeface.recovered'), 'true', 'the failed local clear left a staged marker')
      const reopened = deviceHarness({ storage: app.stored, invitation: 'vibeface1other' })
      try {
        reopened.render(); await settle()
        await reopened.render().connect({ ...entry, device_id: remote })
        assert.equal(reopened.render().recovered, false)
        assert.equal(app.stored.get('vibeface.recovery-wait'), wait)
      } finally { reopened.close() }
    }
  } finally { app.close() }
})

test('boot restores a join started after pausing recovery without replacing either durable intent', async () => {
  const node = { address: entry.address, device_id: target }
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const wait = JSON.stringify({ entry: node, networkId: network, rootKey: network, genesis, checkpoint })
  const storage = new Map([['vibeface.recovery-wait', wait]])
  const app = deviceHarness({ storage })
  const guest = deferred(); guest.resolve({ ...notMember, recovery: null }); app.holds.set('authenticate', guest)
  let pending: string | undefined
  try {
    app.render(); await until(() => /本地找回记录/.test(app.render().error))
    app.render().pauseRecovery()
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    pending = storage.get('vibeface.pending-join')
    assert.equal(storage.get('vibeface.recovery-wait'), wait)
  } finally { app.close() }
  assert.ok(pending)
  const reloaded = deviceHarness({ storage })
  try {
    reloaded.render(); await until(() => reloaded.calls.includes('join_status'))
    assert.equal(reloaded.render().joining?.nonce, JSON.parse(pending).nonce)
    assert.deepEqual(reloaded.calls, [`open:${target}`, 'info', 'join_status'])
    assert.deepEqual(plain(reloaded.sockets[0]?.entry), entry)
    assert.equal(storage.get('vibeface.pending-join'), pending)
    assert.equal(storage.get('vibeface.recovery-wait'), wait)
    reloaded.render().cancelJoin()
    assert.equal(reloaded.calls.filter(call => call === 'join_withdraw').length, 1)
    assert.equal(storage.get('vibeface.pending-join'), 'null')
    assert.equal(storage.get('vibeface.recovery-wait'), wait)
  } finally { reloaded.close() }
})

test('pausing an authenticated recovery lets a saved join own the connection screen', async () => {
  const node = { address: entry.address, device_id: target }
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const wait = JSON.stringify({ entry: node, networkId: network, rootKey: network, genesis, checkpoint })
  const app = deviceHarness({ storage: new Map([['vibeface.recovery-wait', wait]]) })
  const accepted = deferred(); accepted.resolve({ principal: { device_id: subject } }); app.holds.set('authenticate', accepted)
  const write = app.platform.storage.setItem
  app.platform.storage.setItem = (key: string, value: string) => {
    if (key === 'vibeface.recovery-wait' && value === 'null') throw new Error('local store temporarily unavailable')
    write(key, value)
  }
  try {
    app.render(); await until(() => app.render().ready !== null)
    const pending = JSON.stringify({ entry: node, join_id: inviteId, invite_id: inviteId, nonce: joinNonce, name: 'browser' })
    app.stored.set('vibeface.pending-join', pending)
    app.render().pauseRecovery()
    await until(() => app.calls.includes('join_status'))
    assert.equal(app.render().ready, null)
    assert.equal(app.render().joining?.nonce, joinNonce)
    assert.equal(app.render().recovery, null)
    assert.equal(app.stored.get('vibeface.pending-join'), pending)
    assert.equal(app.stored.get('vibeface.recovery-wait'), wait)
    assert.equal(app.calls.includes('recover') || app.calls.includes('join'), false)
  } finally { app.close() }
})

for (const { panel, control, event } of [
  { panel: './recovery-controls.tsx', control: '@vibeface/uiarc/button/button.tsx', event: 'onClick' },
  { panel: '../../../../../apps/mobile/src/features/recovery/recovery.tsx', control: '@/components/arc/button', event: 'onPress' },
] as const) test(`the ${event === 'onClick' ? 'Web' : 'Mobile'} recovery panel lets the user pause a missing receipt and keep its files`, async () => {
  const node = { address: entry.address, device_id: target }
  const checkpoint = recoveryCheckpointFixture([genesis.first_node, genesis.first_client])
  const wait = JSON.stringify({ entry: node, networkId: network, rootKey: network, genesis, checkpoint })
  const savedFile = JSON.stringify({ name: 'vibeface-recovery-ABCDEF12.json', code: 'ABCDEF12', attempted: true, data: 'AQID' })
  const storage = new Map([['vibeface.recovery-wait', wait], ['vibeface.last-authenticated', JSON.stringify({ entry: node, networkId: network })]])
  const tabStorage = new Map([['vibeface.unsaved-recovery', savedFile]])
  const app = deviceHarness({ storage, tabStorage })
  const guest = deferred(); guest.resolve({ ...notMember, recovery: null }); app.holds.set('authenticate', guest)
  try {
    app.render(); await until(() => /本地找回记录/.test(app.render().error))
    assert.equal(app.render().phase, 'reconnecting')
    const pending = JSON.stringify({ entry: node, join_id: inviteId, invite_id: inviteId, nonce: joinNonce, name: 'browser' })
    storage.set('vibeface.pending-join', pending) // Another tab saved a join while this one was waiting for a recovery receipt.
    const actions: (() => unknown)[] = []
    const jsx = (component: unknown, props: Record<string, unknown>) => {
      const callback = props[event]
      if (component === 'arc-button' && props.children === '返回' && typeof callback === 'function') actions.push(() => callback())
      return props.children
    }
    const loadPanel = moduleLoader(createContext({}), specifier => {
      if (specifier === 'react/jsx-runtime') return { jsx, jsxs: jsx }
      if (specifier === 'react') return { useRef: () => ({ current: null }) }
      if (specifier === '@vibeface/client/identity/context.tsx') return { useDevice: app.render }
      if (specifier === '@vibeface/client/identity/copy.ts') return { recoveryCopy: {} }
      if (specifier === control) return { Button: 'arc-button' }
      return {}
    })
    const renderWaiting = loadPanel(fileURLToPath(new URL(panel, import.meta.url))).RecoveryWaiting
    assert.ok(typeof renderWaiting === 'function')
    renderWaiting()
    const back = actions[0]
    assert.ok(back, 'an Arc button is available in the waiting card')
    back()
    await until(() => app.calls.includes('join_status'))
    assert.equal(app.render().recovery, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.equal(app.render().joining?.nonce, joinNonce)
    assert.equal(app.stored.get('vibeface.recovery-wait'), wait)
    assert.equal(tabStorage.get('vibeface.unsaved-recovery'), savedFile)
    assert.equal(app.stored.get('vibeface.pending-join'), pending)
    assert.equal(app.calls.includes('join'), false)
    assert.equal(app.calls.includes('recover'), false)
  } finally { app.close() }
})

test('a local browser reuses its cookie-bound identity and can disable its own automatic access', async () => {
  const app = deviceHarness({ localSession: true })
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    assert.equal(app.render().ready?.principal.can_manage, false)
    assert.equal(app.render().localAutoAccess, true)
    assert.deepEqual(plain(app.sockets[0]?.entry), { kind: 'local', address: 'https://node.example', device_id: target })
    await app.render().setLocalAutoAccess(false)
    assert.equal(app.render().ready, null)
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
    assert.equal(app.calls.filter(method => method === 'local_access').length, 1)
  } finally { app.close() }
})

test('after 断开 a local browser stays disconnected until a launch opens it, then reloads connected again', async () => {
  const storage = new Map<string, string>()
  const first = deviceHarness({ localSession: true, storage })
  try {
    first.render(); await settle()
    assert.equal(first.render().ready?.principal.source, 'local_session')
    first.render().disconnect()
  } finally { first.close() }
  const reopened = deviceHarness({ localSession: true, storage })
  try {
    reopened.render(); await settle()
    assert.equal(reopened.render().ready, null)
    assert.equal(reopened.render().phase, 'disconnected')
    assert.equal(reopened.calls.includes('local-info'), false)
    assert.equal(reopened.sockets.length, 0)
  } finally { reopened.close() }
  const launched = deviceHarness({ localSession: true, storage, fragment: `#local=${'f'.repeat(64)}&node=${target}` })
  try {
    launched.render(); await settle()
    assert.equal(launched.render().ready?.principal.source, 'local_session')
  } finally { launched.close() }
  const reloaded = deviceHarness({ localSession: true, storage })
  try {
    reloaded.render(); await settle()
    assert.equal(reloaded.render().ready?.principal.source, 'local_session')
  } finally { reloaded.close() }
})

test('after 断开 the localhost page shows its computer without opening it', async () => {
  const app = deviceHarness({ local: 'initialized', storage: new Map([['vibeface.stay-disconnected', 'true']]) })
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.render().localSetup?.kind, 'initialized')
    assert.equal(app.sockets.length, 0)
    await app.render().openLocalAccess()
    assert.equal(app.navigations.length, 1)
  } finally { app.close() }
})

test('after 断开 a member on the localhost page reconnects as itself when 打开 is clicked', async () => {
  const storage = new Map([['vibeface.stay-disconnected', 'true'], ['vibeface.connections', JSON.stringify([localEntry])], [`vibeface.network-for.${target}`, JSON.stringify(network)]])
  const app = deviceHarness({ local: 'initialized', management: true, storage })
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.sockets.length, 0)
    await app.render().openLocalAccess(); await settle()
    assert.equal(app.navigations.length, 0)
    assert.equal(app.render().ready?.principal.source, 'membership')
    assert.equal(app.stored.get('vibeface.stay-disconnected'), 'null')
  } finally { app.close() }
})

test('a removed browser on the localhost page opens a local session instead of being refused again', async () => {
  const storage = new Map([['vibeface.connections', JSON.stringify([localEntry])], [`vibeface.network-for.${target}`, JSON.stringify(network)]])
  const app = deviceHarness({ local: 'initialized', removed: true, storage })
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.match(app.render().error, /已被移除/)
    await app.render().openLocalAccess(); await settle()
    assert.equal(app.navigations.length, 1)
    assert.equal(new URL(app.navigations[0]).origin, localLaunch.url)
  } finally { app.close() }
})

test('a signed management browser keeps its permissions through the local cookie channel, reconnect, and reload', async () => {
  const storage = new Map<string, string>()
  for (let reload = 0; reload < 2; reload++) {
    const app = deviceHarness({ localSession: true, management: true, storage })
    try {
      app.render(); await settle()
      assert.equal(app.sockets[0]?.entry.kind, 'local')
      assert.equal(app.render().ready?.principal.source, 'membership')
      assert.equal(app.render().ready?.principal.can_manage, true)
      assert.equal(app.render().ready?.entry.kind, 'gateway')
      assert.deepEqual(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? 'null').entry, { address: 'https://node.example', device_id: target })
      app.sockets[0]?.drop()
      // The newest timer is the reconnect; a manager's directory poll was set before it.
      const [id, retry] = [...app.timers.entries()].at(-1)!
      app.timers.delete(id); retry(); await settle()
      assert.equal(app.sockets[1]?.entry.kind, 'gateway')
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

test('a launch link on the localhost page sends an unjoined browser on to the launcher origin', async () => {
  const app = deviceHarness({ local: 'initialized', fragment: `#local=${'f'.repeat(64)}&node=${target}` })
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.calls.includes('local-exchange'), false)
    assert.equal(app.navigations.length, 1)
    const opened = new URL(app.navigations[0])
    assert.equal(opened.origin, localLaunch.url)
    assert.equal(new URLSearchParams(opened.hash.slice(1)).get('local'), localLaunch.token)
  } finally { app.close() }
})

test('a launch link on the localhost page keeps a browser already joined there', async () => {
  const storage = new Map([['vibeface.last-authenticated', JSON.stringify({ entry, networkId: network })]])
  const app = deviceHarness({ local: 'initialized', storage, fragment: `#local=${'f'.repeat(64)}&node=${target}` })
  app.localState.setup = { kind: 'initialized', removed: false, entry: localEntry, local_launch: localLaunch }
  try {
    app.render(); await settle()
    assert.equal(app.location.hash, '')
    assert.equal(app.calls.includes('local-info'), false)
    assert.equal(app.calls.includes('local-exchange'), false)
    assert.equal(app.navigations.length, 0)
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
      assert.match(app.render().error, /结果还没确认/)
    }
  } finally { app.close() }
})

for (const reply of ['withdrawn', 'lost']) test(`cancelling a waiting join withdraws it once at the entry when the reply is ${reply}`, async () => {
  const app = deviceHarness({ joining: 'pending' })
  const hold = deferred(); app.holds.set('join_withdraw', hold)
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().joining?.join_id, inviteId)
    app.render().cancelJoin()
    assert.equal(app.render().joining, null)
    assert.equal(app.render().phase, 'disconnected')
    assert.equal(app.stored.get('vibeface.pending-join') ?? 'null', 'null')
    if (reply === 'withdrawn') hold.resolve(joinRequest('withdrawn')); else hold.reject(new Error('connection lost'))
    await settle(); await settle()
    assert.deepEqual(app.requests.filter(request => request.method === 'join_withdraw').map(request => request.params.join_id), [inviteId])
    assert.equal(app.render().error, '')
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

for (const stage of ['authenticate', 'info', 'subscribe']) test(`cancelJoin during ${stage} cannot restore the cancelled connection`, async () => {
  const app = deviceHarness({ joining: 'joined' })
  const hold = deferred(); app.holds.set(stage, hold)
  try {
    app.render(); await until(() => { app.render(); return app.calls.includes(stage) })
    app.render().cancelJoin()
    hold.resolve({})
    await settle(); await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().joining, null)
    assert.equal(app.timers.size, 0)
    assert.equal(app.stored.get('vibeface.last-authenticated') ?? 'null', 'null')
    assert.equal(app.calls.filter(call => call.startsWith('open:')).length, 1)
  } finally { app.close() }
})

for (const stage of ['recover', 'subscribe']) test(`cancelSensitive during ${stage} cannot publish ready or restart transport`, async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await settle()
    await app.render().createLocalNetwork('browser', 'test-only-passphrase')
    await app.render().downloadBackup()
    const hold = deferred(); app.holds.set(stage, hold)
    const confirming = app.render().confirmBootstrap(true)
    const rejected = assert.rejects(confirming, /切换|取消/)
    await until(() => app.calls.includes(stage))
    app.render().cancelSensitive()
    hold.resolve({})
    await rejected; await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().review, null)
    assert.equal(app.timers.size, 0)
    assert.equal(app.stored.get('vibeface.last-authenticated') ?? 'null', 'null')
    assert.equal(app.calls.filter(call => call === 'recover').length, 1)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

for (const flow of ['join', 'sensitive']) test(`cancelling ${flow} after subscribe resolves still prevents stale authentication publication`, async () => {
  const app = deviceHarness({ joining: flow === 'join' ? 'confirming' : '', local: flow === 'sensitive' ? 'fresh' : '' })
  try {
    app.render(); await settle(); app.render()
    if (flow === 'join') app.joinStatus.value = joinRequest('joined')
    if (flow === 'sensitive') {
      await app.render().createLocalNetwork('browser', 'test-only-passphrase')
      await app.render().downloadBackup()
    }
    const hold = deferred(); app.holds.set('subscribe', hold)
    const checking = flow === 'join' ? app.render().checkJoin() : app.render().confirmBootstrap(true)
    const rejected = assert.rejects(checking, /切换|取消/)
    await until(() => app.calls.includes('subscribe'))
    hold.resolve({})
    queueMicrotask(() => queueMicrotask(() => queueMicrotask(() => {
      if (flow === 'join') app.render().cancelJoin(); else app.render().cancelSensitive()
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
    const hold = deferred(); app.holds.set(flow === 'directory' ? 'directory' : 'recovery_state', hold)
    let work: Promise<void>
    if (flow === 'directory') work = app.render().refreshDirectory()
    else work = app.render().beginRecovery({ file: new File(['test-fixture'], 'recovery.json'), password: 'test-only-passphrase', name: 'browser', entry })
    const rejected = assert.rejects(work, /切换|取消/)
    await until(() => app.calls.includes(flow === 'directory' ? 'directory' : 'recovery_state'))
    hold.resolve({ state: { genesis }, view: { ...membership, members: [{ device_id: subject, kind: 'client', name: 'browser', manage: false }, { device_id: target, kind: 'node', name: 'node', manage: false }] } })
    queueMicrotask(() => queueMicrotask(() => queueMicrotask(() => app.render().disconnect())))
    await rejected; await settle()
    assert.equal(app.render().phase, 'disconnected')
    assert.equal(app.render().directory, null)
    assert.equal(app.render().review, null)
    assert.equal(app.released(), flow === 'recovery' ? 1 : 0)
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('an approved join restores only verified authentication and subscription, and disconnect cancels retry', async () => {
  const app = deviceHarness({ joining: 'joined' })
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().ready?.entry.device_id, target)
    assert.equal(app.verifications(), 1)
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'join_status', 'join_status', 'authenticate', 'info', 'subscribe', 'entrances'])
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
    assert.equal(app.render().ready?.entry.device_id, target)
    assert.notEqual(app.render().ready?.generation, previous)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.verifications(), 2)
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'authenticate', 'info', 'subscribe', 'entrances', `open:${target}`, 'info', 'authenticate', 'info', 'subscribe', 'entrances'])
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('when its address stops answering, the Node is reached by its device ID through a public gateway it named', async () => {
  const gateway = 'https://gateway.example'
  const app = deviceHarness({ saved: true })
  try {
    const named = deferred(); named.resolve({ entrances: [], gateways: [{ device_id: remote, url: `${gateway}/` }, { device_id: remote, url: 'http://plain.example' }] })
    app.holds.set('entrances', named)
    app.render(); await settle(); app.render()
    assert.deepEqual(JSON.parse(app.stored.get(`vibeface.gateways.${network}`) ?? 'null'), [{ device_id: remote, url: gateway }])
    const down = deferred(); down.promise.catch(() => {}); down.reject(new Error('connection refused'))
    app.holds.set(`open:${entry.address}`, down)
    app.sockets[0].drop()
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    assert.deepEqual(plain(app.sockets.slice(1).map(socket => socket.entry)), [entry, { kind: 'gateway', address: gateway, device_id: target }])
    assert.deepEqual(plain(app.render().ready?.entry), { kind: 'gateway', address: gateway, device_id: target })
  } finally { app.close() }
})

test('a reload resumes the last network through its entrances even with several saved addresses', async () => {
  const other: ConnectionEntry = { kind: 'gateway', address: 'https://other.example', device_id: remote }
  const storage = new Map([['vibeface.connections', JSON.stringify([other, entry])], ['vibeface.last-authenticated', JSON.stringify({ entry, networkId: network })]])
  const app = deviceHarness({ storage })
  try {
    app.render(); await settle()
    assert.equal(app.render().phase, 'reconnecting')
    const [id, resume] = [...app.timers.entries()][0]
    app.timers.delete(id); resume(); await settle()
    assert.equal(app.render().ready?.entry.device_id, target)
    assert.deepEqual(app.calls.slice(0, 3), [`open:${target}`, 'info', 'authenticate'])
  } finally { app.close() }
})

test('a reload while the Node is down shows the browser offline, and retrying now tries again at once', async () => {
  const storage = new Map([['vibeface.connections', JSON.stringify([entry])], ['vibeface.last-authenticated', JSON.stringify({ entry, networkId: network })]])
  const app = deviceHarness({ storage })
  try {
    app.render(); await settle()
    const down = deferred(); app.holds.set('open', down)
    const [id, resume] = [...app.timers.entries()][0]
    app.timers.delete(id); resume(); await settle()
    down.reject(new Error('connection refused')); await settle()
    const link = app.render().link
    assert.equal(link.kind, 'offline', 'a first load that cannot reach the Node says so instead of connecting forever')
    assert.equal(app.render().phase, 'reconnecting')
    assert.equal(app.timers.size, 1)
    app.holds.delete('open')
    app.render().retryNow(); await settle()
    const [now, again] = [...app.timers.entries()][0]
    app.timers.delete(now); again(); await settle()
    assert.equal(app.render().ready?.entry.device_id, target)
  } finally { app.close() }
})

const pageAccess = (app: ReturnType<typeof deviceHarness>) => {
  const device = app.render()
  return accessOf({ ready: Boolean(device.ready), phase: device.phase, joining: Boolean(device.joining), savedEntries: device.entries.length, invitation: Boolean(device.initialInvitation), offlineRetryMs: null }).kind
}

test('a Node error while reconnecting keeps the credentials and the page, and retries', async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    const saved = app.stored.get('vibeface.last-authenticated')
    app.sockets[0].drop()
    const hold = deferred(); app.holds.set('authenticate', hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    hold.reject(Object.assign(new Error('database is locked'), { responded: true })); await settle()
    assert.equal(app.render().phase, 'reconnecting')
    assert.equal(app.render().ready?.isCurrent(), false)
    assert.equal(pageAccess(app), 'member', 'the workspace stays on screen with what it last showed')
    assert.equal(app.stored.get('vibeface.last-authenticated'), saved)
    assert.equal(app.timers.size, 1)
    app.holds.delete('authenticate')
    const [next, again] = [...app.timers.entries()][0]
    app.timers.delete(next); again(); await settle()
    assert.equal(app.render().ready?.entry.device_id, target)
    assert.equal(pageAccess(app), 'member')
  } finally { app.close() }
})

test('a revoked browser stops reconnecting, forgets the network and goes to the connect page', async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    app.sockets[0].drop()
    const hold = deferred(); app.holds.set('authenticate', hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    hold.resolve({ ...notMember, revoked: true }); await settle()
    assert.equal(app.render().ready, null)
    assert.equal(app.render().phase, 'unpaired')
    assert.equal(pageAccess(app), 'auth')
    assert.match(app.render().error, /已被移除/)
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
    assert.equal(app.timers.size, 0)
    app.sockets[1].drop(); assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

const leaves = (app: ReturnType<typeof deviceHarness>) => app.requests.filter(request => request.method === 'revoke').map(request => plain(request.params.event))

test('断开 revokes this browser\'s own key, and the next join offers a new one', async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    await app.render().leave()
    assert.deepEqual(leaves(app), [{ issuer: subject, action: { type: 'revoke', device_id: subject, cutoff_seq: 0, cutoff_hash: null }, signature: 'signature' }])
    assert.equal(app.render().ready, null)
    assert.equal(app.render().identity?.deviceId, freshKey)
    assert.equal(app.stored.get('vibeface.stay-disconnected'), 'true')
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
    app.sockets[0].drop(); assert.equal(app.timers.size, 0, 'the closed channel of a left device does not reconnect')
    app.joinStatus.value = joinRequest('pending', { device_id: freshKey })
    await app.render().join('vibeface://join/vibeface1ticket', 'browser')
    assert.deepEqual(app.requests.filter(request => request.method === 'join').map(request => request.params.public_key), [freshKey])
    assert.equal(app.calls.filter(call => call === 'replace-key').length, 1)
  } finally { app.close() }
})

for (const landed of [true, false]) test(`a lost 断开 reply asks the Node whether it ${landed ? 'landed' : 'did not land'} and never signs again`, async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    const reply = deferred(); app.holds.set('revoke', reply)
    const leaving = app.render().leave()
    await settle()
    if (landed) app.holds.set('authenticate', { promise: Promise.resolve({ ...notMember, revoked: true }), resolve() {}, reject() {} })
    app.sockets[0].drop(); reply.reject(new Error('connection lost'))
    if (landed) {
      await leaving
      assert.equal(app.render().identity?.deviceId, freshKey)
      assert.equal(app.render().ready, null)
    } else {
      await assert.rejects(leaving, /connection lost/)
      assert.equal(app.render().identity?.deviceId, subject)
      assert.equal(app.render().ready?.isCurrent(), true, 'a device still in the network stays connected')
      assert.equal(app.calls.includes('replace-key'), false)
    }
    assert.equal(leaves(app).length, 1)
    assert.equal(app.sockets.length, 2)
  } finally { app.close() }
})

test('the only management device is told to hand over management before 断开', async () => {
  const app = deviceHarness({ saved: true, management: true })
  try {
    app.render(); await settle(); app.render()
    const verify = app.secure.verifyState
    app.secure.verifyState = () => ({ ...verify(), governanceTemplate: () => { throw new Error('the network needs at least one management device') } })
    await assert.rejects(app.render().leave(), /这个浏览器是唯一的管理设备，先把另一台设为管理设备再断开/)
    assert.deepEqual(leaves(app), [])
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.calls.includes('replace-key'), false)
  } finally { app.close() }
})

test('断开 on a local session only closes it: the browser was never a member', async () => {
  const app = deviceHarness({ localSession: true })
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    await app.render().leave()
    assert.deepEqual(leaves(app), [])
    assert.equal(app.render().ready, null)
    assert.equal(app.calls.includes('replace-key'), false)
  } finally { app.close() }
})

test('successful bootstrap releases local material without disconnecting the authenticated session', async () => {
  const app = deviceHarness({ local: 'fresh' })
  try {
    app.render(); await settle()
    await app.render().createLocalNetwork('browser', 'test-only-passphrase')
    await app.render().downloadBackup()
    await app.render().confirmBootstrap(true)
    assert.equal(app.render().ready?.entry.device_id, target)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.render().review, null)
    assert.equal(app.calls.filter(call => call === 'recover').length, 1)
    assert.equal(app.verifications(), 1)
    assert.equal(app.released(), 1)
  } finally { app.close() }
})

for (const original of ['auto-authenticate', 'join-poll', 'waiting-info']) test(`late ${original} failure cannot overwrite another connection`, async () => {
  const app = deviceHarness({ saved: original === 'auto-authenticate', joining: original !== 'auto-authenticate' ? 'pending' : '' })
  try {
    const hold = deferred(), method = original === 'auto-authenticate' ? 'authenticate' : original === 'join-poll' ? 'join_status' : 'info'
    app.holds.set(method, hold)
    app.render(); await until(() => { app.render(); return app.calls.includes(method) })
    app.render().disconnect()
    app.holds.delete(method)
    const replacement = { ...entry, device_id: 'f'.repeat(64) }
    await app.render().connect(replacement)
    hold.reject(Object.assign(new Error('old permission rejected'), { responded: true }))
    await settle()
    assert.equal(app.render().ready?.entry.device_id, replacement.device_id)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.render().phase, 'ready')
    assert.equal(app.render().error, '')
  } finally { app.close() }
})

for (const stage of ['open', 'authenticate']) test(`switching devices during reconnect ${stage} invalidates the old retry`, async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().ready?.entry.device_id, target)
    app.sockets[0].drop(); app.sockets[0].drop()
    assert.equal(app.timers.size, 1)
    const hold = deferred(); app.holds.set(stage, hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry()
    await settle()
    assert.equal(app.calls.filter(call => call === `open:${target}`).length, 2)
    app.render().disconnect()
    app.holds.delete(stage)
    const replacement = { ...entry, device_id: 'f'.repeat(64) }
    await app.render().connect(replacement)
    hold.resolve({})
    await settle()
    assert.equal(app.render().ready?.entry.device_id, replacement.device_id)
    assert.equal(app.render().phase, 'ready')
    assert.equal(app.render().error, '')
    assert.equal(app.timers.size, 0)
    assert.equal(app.calls.filter(call => call === `open:${target}`).length, 2)
    assert.equal(app.calls.filter(call => ['recover', 'join', 'rpc'].includes(call)).length, 0)
  } finally { app.close() }
})

for (const stage of ['open', 'info', 'authenticate']) for (const outcome of ['reject', 'success', 'disconnect', 'unmount', 'manual']) test(`a tab entrance during ${stage} ${outcome} only resumes the current connection`, async () => {
  const app = deviceHarness({ saved: true })
  app.enableTabs()
  const next = { ...entry, address: 'https://new-entry.example' }
  const manual = { ...entry, address: 'https://manual-entry.example' }
  try {
    app.render(); await settle()
    app.sockets[0].drop()
    const hold = deferred()
    app.holds.set(stage, hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry(); await settle()
    const attempts = app.sockets.length
    app.broadcast({ type: 'entrance', networkId: network, entry: next })
    app.broadcast({ type: 'entrance', networkId: network, entry: next })
    assert.equal(app.timers.size, 0, 'broadcasts wait for the active attempt')
    if (outcome === 'disconnect') app.render().disconnect()
    if (outcome === 'unmount') app.close()
    app.holds.delete(stage)
    if (outcome === 'manual') await app.render().connect(manual)
    if (outcome === 'success') hold.resolve(stage === 'info' ? { device_id: target, name: 'Node', version: 'test', initialized: true, network_id: null } : stage === 'authenticate' ? { principal: { network_id: network } } : undefined)
    else hold.reject(new Error('controlled old entrance failure'))
    await settle()
    if (outcome === 'reject') {
      assert.equal(app.timers.size, 1)
      const [nextId, reconnect] = [...app.timers.entries()][0]
      app.timers.delete(nextId); reconnect(); await settle()
      const active = app.render().ready?.entry
      assert.equal(active?.kind !== 'lan' && active?.address, next.address)
      assert.equal(app.sockets.length, attempts + 1)
      assert.equal(app.timers.size, 0)
    } else if (outcome === 'success') {
      assert.equal(app.render().ready?.isCurrent(), true)
      assert.equal(app.timers.size, 0)
      assert.equal(app.sockets.length, attempts)
    } else {
      assert.equal(app.timers.size, 0)
      assert.equal(app.sockets.length, attempts + (outcome === 'manual' ? 1 : 0))
      if (outcome === 'manual') { const active = app.render().ready?.entry; assert.equal(active?.kind !== 'lan' && active?.address, manual.address) }
    }
    assert.equal(app.requests.some(({ method }) => !['info', 'authenticate', 'subscribe', 'entrances'].includes(method)), false, 'reconnect never replays business writes')
  } finally { app.close() }
})

for (const flow of ['recovery', 'rotation']) test(`${flow} persists the same sealed copy before submission and can share it after remount`, async () => {
  const app = deviceHarness({ saved: true }), kept = new Map<string, string>(), handed: Uint8Array[] = []
  let fail = true
  const storage = { read: () => kept.get('copy') ?? null, write(value: string | null) { if (fail) throw new Error('private disk full'); if (value === null) kept.delete('copy'); else kept.set('copy', value) } }
  app.platform.recoveryStorage = storage
  app.platform.saveRecovery = async (bytes) => { handed.push(bytes.slice()); throw new Error('share dismissed') }
  try {
    app.render(); await settle()
    const input = () => ({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', oldPassword: 'test-only-passphrase', name: 'phone', entry })
    if (flow === 'recovery') await app.render().beginRecovery(input())
    const submit = () => { const review = app.render().review; return flow === 'rotation' ? app.render().replaceRecoveryFile(input()) : app.render().confirmRecovery(review?.kind === 'recovery' ? review.members : []) }
    await assert.rejects(submit(), /private disk full/)
    assert.equal(app.calls.includes('recover'), false)
    assert.equal(handed.length, 0)
    fail = false
    await submit()
    assert.equal(app.calls.filter(method => method === 'recover').length, 1)
    assert.equal(handed.length, 1)
    const value = storage.read()
    assert.ok(value)
    assert.equal(/test-only-passphrase|private-test-key/.test(value), false)
    const reloaded = deviceHarness()
    reloaded.platform.recoveryStorage = storage
    reloaded.platform.saveRecovery = (bytes) => { handed.push(bytes.slice()) }
    try {
      reloaded.render(); await settle()
      assert.equal(reloaded.render().unsavedRecovery?.attempted, false)
      await reloaded.render().downloadUnsavedRecovery()
      assert.deepEqual(handed[1], handed[0])
      assert.equal(reloaded.render().unsavedRecovery?.attempted, true)
      reloaded.render().forgetUnsavedRecovery()
      assert.equal(storage.read(), null)
    } finally { reloaded.close() }
  } finally { app.close() }
})

test('a local erase failure stays retryable and a pending erase never reconnects after restart', async () => {
  const app = deviceHarness({ saved: true })
  let attempts = 0
  app.platform.erase = async () => { if (++attempts === 1) throw new Error('controlled delete failure'); app.stored.clear() }
  try {
    app.render(); await settle()
    await assert.rejects(app.render().factoryReset(), /controlled delete failure/)
    assert.equal(app.render().phase, 'erase_failed')
    assert.equal(app.restarts(), 0)
    assert.equal(app.stored.get('vibeface.erase-pending'), 'true')
    const reloaded = deviceHarness({ storage: app.stored })
    try { reloaded.render(); await settle(); assert.equal(reloaded.render().phase, 'erase_failed'); assert.equal(reloaded.sockets.length, 0) } finally { reloaded.close() }
    await app.render().factoryReset()
    assert.equal(app.restarts(), 1)
  } finally { app.close() }
})

test('web sealed recovery copies survive a tab reload and disappear with that tab', async () => {
  const tabStorage = new Map<string, string>(), app = deviceHarness({ saved: true, tabStorage })
  try {
    app.render(); await settle()
    await app.render().replaceRecoveryFile({ file: new File(['fixture'], 'recovery.json'), password: 'test-only-passphrase', oldPassword: 'test-only-passphrase' })
    assert.equal(app.render().unsavedRecovery?.attempted, true)
    const reload = deviceHarness({ tabStorage }), newTab = deviceHarness()
    try {
      assert.equal(reload.render().unsavedRecovery?.code, app.render().unsavedRecovery?.code)
      assert.equal(newTab.render().unsavedRecovery, null)
      reload.render().forgetUnsavedRecovery()
      assert.equal(tabStorage.size, 0)
    } finally { reload.close(); newTab.close() }
  } finally { app.close() }
})

test('a failed remote leave still allows the authorized local erase', async () => {
  const app = deviceHarness({ saved: true }), refusal = deferred()
  let erased = false
  app.platform.erase = async () => { erased = true; app.stored.clear() }
  try {
    app.render(); await settle()
    app.holds.set('revoke', refusal)
    refusal.promise.catch(() => {}); refusal.reject(new Error('controlled remote refusal'))
    await app.render().factoryReset()
    assert.equal(erased, true)
    assert.equal(app.restarts(), 1)
    assert.equal(app.requests.filter(request => request.method === 'revoke').length, 1)
  } finally { app.close() }
})
