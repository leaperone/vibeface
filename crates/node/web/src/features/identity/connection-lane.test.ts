import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext, Script } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'
import type { ConnectionEntry } from '@vibeface/client/lib/node.ts'
import { primaryLane, shouldAutoConnect } from '@vibeface/client/identity/connection-lane.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'

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

const target = 'a'.repeat(64), network = 'b'.repeat(64), subject = 'c'.repeat(64)
const entry: ConnectionEntry = { address: 'https://node.example', device_id: target }
const localEntry: ConnectionEntry = { address: 'http://localhost:4790', device_id: target }
const localCapability = 'f'.repeat(64)
const localInvitation = { url: localEntry.address, device_id: target, capability: localCapability }
const localLaunch = { url: 'http://127.0.0.1:49781', device_id: target, token: 'e'.repeat(64) }
const genesis = { version: 1, network_id: network, first_client: { device_id: subject, kind: 'client', name: 'browser', manage: true }, first_node: { device_id: target, kind: 'node', name: 'node', manage: false }, signature: 's' }
const membership = { network_id: network, recovery_key: network, members: [], revoked: [], heads: [], epoch: 1, attesters: [] }
const inviteId = '1'.repeat(32), joinNonce = '9'.repeat(64)
const joinRequest = (status: string, extra: Record<string, unknown> = {}) => ({ join_id: 'join', status, network_id: network, invite_id: inviteId, device_id: subject, kind: 'client', name: 'browser', commit: 'd'.repeat(64), admin_id: null, admin_nonce: null, joiner_nonce: null, reason: null, expires_at_ms: Date.now() + 60000, ...extra })
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
function deviceHarness({ joining = '', bootstrap = false, saved = false, local = '', invitation = '', localSession = false, management = false, fragment = '', storage = new Map<string, string>(), removed = false } = {}) {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const afterRender: (() => void)[] = [], timers = new Map<number, () => void>()
  const stored = storage, calls: string[] = [], navigations: string[] = []
  const requests: { method: string; params: Record<string, unknown> }[] = []
  const setupRequests: { path: string; options: RequestInit }[] = []
  const joinStatus = { value: joining ? joinRequest(joining) : joinRequest('pending') } as { value: Record<string, unknown> }
  const managed = { value: [] as Record<string, unknown>[] }
  const holds = new Map<string, ReturnType<typeof deferred>>()
  const sockets: { entry: ConnectionEntry; close(): void; drop(): void }[] = []
  const sessionEntry: ConnectionEntry = { address: 'https://node.example', device_id: target, local: true }
  const localState: { enabled: boolean; setup?: unknown } = { enabled: true }
  const pageOrigin = local ? localEntry.address : entry.address
  const hash = invitation ? `#${invitation}` : fragment, pathname = invitation ? '/join' : '/'
  const location = { hash, pathname, href: `${pageOrigin}${pathname}${hash}`, origin: pageOrigin, assign(url: string) { navigations.push(url) } }
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
  if (joining) stored.set('vibeface.pending-join', JSON.stringify({ entry, join_id: 'join', invite_id: inviteId, nonce: joinNonce, name: 'browser', pinned: { network_id: network, admin_id: target, admin_nonce: '2'.repeat(64) } }))
  if (saved) stored.set('vibeface.connections', JSON.stringify([entry]))
  // A cookie-bound local session only exists after a launch on this origin, which the browser remembers.
  if (localSession) stored.set('vibeface.local-origin', 'true')
  let key = subject
  const transport = {
    validTarget: (value: string) => value, validEndpoint: (value: string) => value,
    initClient: async () => ({ deviceId: key }), signClient: async () => 'signature',
    replaceClient: async () => { calls.push('replace-key'); key = freshKey; return { deviceId: key } },
    parseJoinLink: async () => ({ origin: entry.address, entryId: target, inviteId, inviteHash: 'h'.repeat(64), secret: 'one-time-secret' }),
    async connectDevice(candidate: ConnectionEntry, options: { onClose(cause: Error): void; signal: AbortSignal }) {
      calls.push(`open:${candidate.device_id}`)
      let closed = false
      const close = () => { if (closed) return; closed = true; options.onClose(new Error('connection lost')) }
      sockets.push({ entry: candidate, close, drop: close })
      options.signal.addEventListener('abort', close)
      if (holds.has('open')) await holds.get('open')?.promise
      return { deviceId: candidate.device_id, close, async request(method: string, params: Record<string, unknown> = {}) {
        if (method === 'log') return { accepted: 1 }
        calls.push(method)
        requests.push({ method, params })
        if (holds.has(method)) return holds.get(method)?.promise
        const initialized = !bootstrap && local !== 'fresh' && candidate.device_id !== remote
        if (method === 'info') return { device_id: candidate.device_id, name: 'node', version: 'test', initialized, network_id: initialized ? network : null }
        if (method === 'authenticate' && localSession) return { principal: { network_id: network } }
        if (method === 'authenticate' && removed && key === subject) return { principal: null, initialized: true, network_id: network, revoked: true }
        if (method === 'local_access') { localState.enabled = false; return { enabled: false } }
        if (method === 'join') return joinRequest('pending', { device_id: params.public_key })
        if (method === 'join_status' && !params.join_id) return { requests: managed.value }
        if (method === 'approve') return joinRequest('confirming', { device_id: remote, admin_id: subject, admin_nonce: params.admin_nonce })
        if (method === 'directory') return { state: { genesis }, view: secure.verifiedView() }
        if (method === 'join_status') return params.reveal ? { ...joinStatus.value, joiner_nonce: params.reveal } : joinStatus.value.status === 'joined' ? { ...joinStatus.value, state: { genesis } } : joinStatus.value
        if (method === 'recovery_state' || method === 'directory') return { state: { genesis, checkpoints: [] }, view: membership, heads: {} }
        return {}
      } }
    },
  }
  let verifications = 0, released = 0
  const secure = {
    verifyAuthentication: () => { verifications++; return { auth: { local_auto_access: true }, principal: { source: localSession && !management ? 'local_session' : 'membership', network_id: network, device_id: subject, epoch: 1, can_manage: management }, view: membership } },
    verifyRemoval: () => false,
    verifiedView: () => ({ ...membership, members: [{ device_id: subject, kind: 'client', name: 'browser', manage: false }] }),
    openPackage: async () => ({ networkId: network, material: { genesis, checkpoints: [] }, root: { deviceId: () => network, sign: () => 'signature' }, release() { released++ } }),
    Identity: { generate: () => ({ deviceId: () => network, secretHex: () => 'private-test-key', sign: () => 'signature', free() { released++ } }) },
    createGenesis: () => JSON.stringify(genesis), joinCommit: () => 'd'.repeat(64), shortCode: () => '123456', ticketEncode: () => 'vibeface1test',
    governancePayload: () => new Uint8Array(), inviteSigningPayload: () => new Uint8Array(),
    sealPackage: () => new Uint8Array([1, 2, 3]),
    verifyState: () => ({ free() {}, viewJson: () => JSON.stringify(membership), governanceTemplate: () => '{}', createRecoveryCheckpoint: () => JSON.stringify({ epoch: 2 }), createKeyRotation: () => JSON.stringify({ epoch: 2 }) }),
  }
  const timer = (run: () => void) => { const id = ++nextTimer; timers.set(id, run); return id }
  const sandbox = createContext({
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    location, history: { replaceState(_state: unknown, _title: string, url: URL) { location.hash = url.hash } }, URL, URLSearchParams, AbortController, TextEncoder, TextDecoder, Uint8Array, Error, crypto: globalThis.crypto,
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
  const modules = new Map<string, { exports: Record<string, unknown> }>(), sourceRoot = fileURLToPath(new URL('../../', import.meta.url))
  const clientRoot = fileURLToPath(new URL('../../../../../../packages/client/src/', import.meta.url))
  function load(filename: string): Record<string, unknown> {
    const cached = modules.get(filename)
    if (cached) return cached.exports
    const module = { exports: {} }; modules.set(filename, module)
    const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
    const require = (specifier: string) => {
      if (specifier === 'react') return react
      if (specifier === 'react/jsx-runtime') return { jsx: (_type: unknown, props: unknown) => props }
      if (specifier.endsWith('/lib/transport.ts')) return transport
      if (specifier.endsWith('/lib/secure.ts')) return { secureKit: () => secure }
      const path = specifier.startsWith('@/') ? resolve(sourceRoot, specifier.slice(2)) : specifier.startsWith('@vibeface/client/') ? resolve(clientRoot, specifier.slice(17)) : resolve(dirname(filename), specifier)
      return load(extname(path) ? path : `${path}.ts`)
    }
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(module.exports, require, module)
    return module.exports
  }
  const Provider = load(resolve(clientRoot, 'identity/context.tsx')).DeviceProvider
  const platform = { secure: {}, transport, storage: sandbox.localStorage, page: load(resolve(sourceRoot, 'lib/local-page.ts')).localPage, self: '这个浏览器', saveRecovery() {} }
  assert.equal(typeof Provider, 'function')
  type Device = ReturnType<typeof import('@vibeface/client/identity/context.tsx').useDevice>
  const render = (): Device => {
    cursor = 0
    assert.ok(typeof Provider === 'function')
    const props = Provider({ platform, children: null })
    afterRender.splice(0).forEach(run => run())
    return props.value
  }
  return { render, calls, requests, setupRequests, joinStatus, managed, holds, sockets, stored, timers, location, localState, navigations, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

const notMember = { principal: null, initialized: true, network_id: network, revoked: false }

test('fresh localhost discovers its one-time capability and creates the network through recover', async () => {
  const app = deviceHarness({ local: 'fresh', saved: true })
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
    assert.equal(app.render().ready?.entry.address, localEntry.address)
    assert.equal(app.render().localSetup?.kind, 'initialized')
    const recover = app.requests.filter(request => request.method === 'recover')
    assert.equal(recover.length, 1)
    assert.equal(recover[0].params.capability, localCapability)
    assert.deepEqual(plain(recover[0].params.genesis), genesis)
  } finally { app.close() }
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
      assert.match(app.render().error, /本机链接和这台电脑对不上/)
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
    assert.equal(app.render().initialInvitation, `${localEntry.address}/join#vibeface1ticket`)
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

test('joining commits first, reveals only after the manager nonce, then shows the short code and authenticates', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await app.render().join('https://node.example/join#vibeface1ticket', 'browser')
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

test('a removed browser joins again as a new device and never offers its revoked key', async () => {
  const app = deviceHarness({ removed: true })
  try {
    app.render(); await settle()
    app.joinStatus.value = joinRequest('pending', { device_id: freshKey })
    await app.render().join('https://node.example/join#vibeface1ticket', 'browser')
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

test('a pending join survives reload without resubmitting', async () => {
  const app = deviceHarness({ joining: 'pending' })
  try {
    app.render(); await settle()
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'join_status'])
    assert.equal(app.render().joining?.join_id, 'join')
  } finally { app.close() }
})

test('a joiner keeps the manager nonce it saw before revealing and stops when the entry changes it', async () => {
  const app = deviceHarness()
  try {
    app.render(); await settle()
    await app.render().join('https://node.example/join#vibeface1ticket', 'browser')
    const nonce = JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null').nonce
    app.joinStatus.value = joinRequest('confirming', { admin_id: target, admin_nonce: '2'.repeat(64) })
    await app.render().checkJoin()
    assert.equal(app.render().joining?.code, '123456')
    assert.equal(JSON.parse(app.stored.get('vibeface.pending-join') ?? 'null').pinned.admin_nonce, '2'.repeat(64))
    app.joinStatus.value = joinRequest('confirming', { admin_id: target, admin_nonce: '3'.repeat(64), joiner_nonce: nonce })
    await assert.rejects(app.render().checkJoin(), /改动了加入请求/)
    assert.equal(app.render().joining, null)
  } finally { app.close() }
})

test('a manager shows the short code only for its own nonce and the commitment it saw before sending it', async () => {
  const app = deviceHarness({ management: true, saved: true })
  try {
    app.render(); await settle(); app.render()
    app.managed.value = [joinRequest('pending', { device_id: remote })]
    await app.render().refreshDirectory(); app.render()
    await app.render().startApproval('join')
    const sent = app.requests.filter(request => request.method === 'approve').map(request => request.params.admin_nonce)
    const revealed = { device_id: remote, admin_id: subject, admin_nonce: sent[0], joiner_nonce: joinNonce }
    for (const [change, code] of [[{}, '123456'], [{ commit: 'e'.repeat(64) }, undefined], [{ admin_nonce: '3'.repeat(64) }, undefined], [{ device_id: '7'.repeat(64) }, undefined]] as const) {
      app.managed.value = [joinRequest('confirming', { ...revealed, ...change })]
      await app.render().refreshDirectory()
      assert.equal(app.render().joins[0]?.code, code)
    }
    await app.render().startApproval('join')
    assert.deepEqual(app.requests.filter(request => request.method === 'approve').map(request => request.params.admin_nonce), [sent[0], sent[0]])
  } finally { app.close() }
})

test('a join receipt for another invite or device is refused', async () => {
  for (const mismatch of [{ invite_id: '2'.repeat(32) }, { device_id: '5'.repeat(64) }]) {
    const app = deviceHarness()
    try {
      app.render(); await settle()
      app.holds.set('join', { promise: Promise.resolve(joinRequest('pending', mismatch)), resolve() {}, reject() {} })
      await assert.rejects(app.render().join('https://node.example/join#vibeface1ticket', 'browser'), /不一致/)
      assert.equal(app.render().joining, null)
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
    assert.deepEqual(plain(recover?.params.checkpoints), [{ epoch: 2 }])
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

test('a local browser reuses its cookie-bound identity and can disable its own automatic access', async () => {
  const app = deviceHarness({ localSession: true })
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    assert.equal(app.render().ready?.principal.can_manage, false)
    assert.equal(app.render().localAutoAccess, true)
    assert.equal(app.sockets[0]?.entry.address, 'https://node.example')
    assert.equal(app.sockets[0]?.entry.device_id, target)
    assert.equal(app.sockets[0]?.entry.local, true)
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

for (const stage of ['authenticate', 'subscribe']) test(`cancelJoin during ${stage} cannot restore the cancelled connection`, async () => {
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
    for (let tries = 0; tries < 100 && !app.calls.includes(flow === 'directory' ? 'directory' : 'recovery_state'); tries++) await new Promise(resolve => setTimeout(resolve, 1))
    assert.ok(app.calls.includes(flow === 'directory' ? 'directory' : 'recovery_state'))
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

test('an approved join restores only verified authentication and subscription, and disconnect cancels retry', async () => {
  const app = deviceHarness({ joining: 'joined' })
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().ready?.entry.device_id, target)
    assert.equal(app.verifications(), 1)
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'join_status', 'join_status', 'authenticate', 'subscribe', 'entrances'])
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
    assert.deepEqual(app.calls, [`open:${target}`, 'info', 'authenticate', 'subscribe', 'entrances', `open:${target}`, 'info', 'authenticate', 'subscribe', 'entrances'])
    assert.equal(app.timers.size, 0)
  } finally { app.close() }
})

test('a reload resumes the last network through its entrances even with several saved addresses', async () => {
  const other: ConnectionEntry = { address: 'https://other.example', device_id: remote }
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
