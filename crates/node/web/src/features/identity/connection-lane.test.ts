import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext, Script } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'
import { createHash, generateKeyPairSync, createPublicKey, sign as signBytes, verify as verifyBytes } from 'node:crypto'
import type { ConnectionEntry, MembershipView, Member } from '../../lib/node.ts'
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

test('invitation transport endpoints preserve custom WSS paths while links use the HTTPS page', () => {
  const endpoint = 'wss://relay.example/custom/ws'
  const content = JSON.stringify({ url: endpoint, peer_id: target, relay: true, capability: 'bootstrap' })
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
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 3 }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 0 }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1 }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2 }), 'saved')
})

const target = 'a'.repeat(64), cluster = 'b'.repeat(64), subject = 'c'.repeat(64), publicKey = 'd'.repeat(64)
const entry: ConnectionEntry = { address: 'https://node.example', peer_id: target, relay: true }
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

function deviceHarness({ pairing = false, bootstrap = false, saved = false, local = false, fragment = '' } = {}) {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const afterRender: (() => void)[] = [], timers = new Map<number, () => void>()
  const stored = new Map<string, string>(), calls: string[] = []
  const discovery: { records: { peerId: string; publicKey: string; expiresAt: number; entries: ConnectionEntry[] }[]; signals: AbortSignal[]; rejected: Set<string> } = { records: [], signals: [], rejected: new Set() }
  const signed: { view: MembershipView; mergeError?: Error } = { view: membership }
  const holds = new Map<string, ReturnType<typeof deferred>>()
  const sockets: { entry: ConnectionEntry; close(): void; drop(): void }[] = []
  const localEntry = { address: 'https://node.example', peer_id: target, relay: false, local: true }
  const localState = { enabled: true }
  const location = { hash: fragment, href: `https://node.example/${fragment}`, origin: 'https://node.example' }
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
    async connectDevice(candidate: ConnectionEntry, options: { onClose(cause: Error): void; signal: AbortSignal }) {
      calls.push(`open:${candidate.peer_id}`)
      let closed = false
      const close = () => { if (closed) return; closed = true; options.onClose(new Error('connection lost')) }
      sockets.push({ entry: candidate, close, drop: close })
      options.signal.addEventListener('abort', close)
      if (holds.has('open')) await holds.get('open')?.promise
      return { peerId: candidate.peer_id, publicKey, close, async request(method: string) {
        calls.push(method)
        if (holds.has(method)) { const held = await holds.get(method)?.promise; if (method !== 'authenticate') return held }
        if (method === 'info') return { peer_id: candidate.peer_id, public_key: publicKey, version: 'test', roles: ['gateway', 'replica'], initialized: !bootstrap }
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
    authorizeMembership: () => { if (signed.view.revoked.includes(subject)) throw new secure.MembershipRevokedError('signed revocation'); return { manage: false } },
    MembershipRevokedError: class extends Error {},
    verifyAuthentication: () => { verifications++; if (signed.view.revoked.includes(subject)) throw new secure.MembershipRevokedError('signed revocation'); return { auth: { local_auto_access: true }, principal: { source: local ? 'local_session' : 'membership', cluster_id: cluster, subject_id: subject, epoch: 1, can_manage: false }, view: signed.view } },
    verifiedView: () => signed.view,
    openPackage: async () => ({ clusterId: cluster, epoch: 1, payload: { genesis }, root: { publicKeyHex: () => publicKey, sign: () => 'signature' }, release() { released++ } }),
    Identity: { generate: () => ({ peerId: () => cluster, publicKeyHex: () => publicKey, secretHex: () => 'private-test-key', sign: () => 'signature', free() { released++ } }) },
    sealPackage: () => new Uint8Array([1, 2, 3]), downloadPackage() {}, verifyState: () => ({ free() {}, viewJson: () => JSON.stringify(membership) }),
  }
  const timer = (run: () => void) => { const id = ++nextTimer; timers.set(id, run); return id }
  const sandbox = createContext({
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    location, history: { replaceState(_state: unknown, _title: string, url: URL) { location.hash = url.hash } }, URL, URLSearchParams, AbortController, TextEncoder, Uint8Array, Error, crypto: globalThis.crypto,
    fetch: async (url: string, options: RequestInit) => {
      if (!local) return { ok: false }
      if (url === '/api/local/info') return { ok: true, json: async () => ({ enabled: localState.enabled, has_session: localState.enabled, subject_id: subject, cluster_id: cluster, entry: localEntry }) }
      assert.equal(url, '/api/local/exchange')
      assert.equal(options.credentials, 'same-origin')
      assert.equal(options.redirect, 'error')
      assert.deepEqual(JSON.parse(String(options.body)), { token: 'f'.repeat(64), peer_id: target, subject_id: subject, public_key: publicKey })
      calls.push('local-exchange')
      return { ok: true, json: async () => ({ cluster_id: cluster, entry: localEntry }) }
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
  return { render, calls, holds, discovery, signed, sockets, stored, timers, location, localState, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

test('a local browser reuses its cookie-bound identity and can disable its own automatic access', async () => {
  const app = deviceHarness({ local: true })
  try {
    app.render(); await settle()
    assert.equal(app.render().ready?.principal.source, 'local_session')
    assert.equal(app.render().ready?.principal.can_manage, false)
    assert.equal(app.render().localAutoAccess, true)
    assert.equal(app.calls.filter(method => method === 'pair_request').length, 0)
    assert.equal(app.sockets[0]?.entry.address, 'https://node.example')
    assert.equal(app.sockets[0]?.entry.peer_id, target)
    assert.equal(app.sockets[0]?.entry.local, true)
    assert.equal(app.sockets[0]?.entry.relay, false)
    await app.render().setLocalAutoAccess(false)
    assert.equal(app.render().ready, null)
    assert.equal(app.stored.get('vibeface.last-authenticated'), 'null')
    assert.equal(app.calls.filter(method => method === 'local_access').length, 1)
  } finally { app.close() }
})

test('the launcher fragment is cleared before its token is exchanged for the browser key', async () => {
  const app = deviceHarness({ local: true, fragment: `#local=${'f'.repeat(64)}&node=${target}` })
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
  const app = deviceHarness({ local: true })
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
  const app = deviceHarness({ local: true })
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
      const challenge = { cluster_id: cluster, owner_public_key: publicKey, subject_id: subject, target_peer_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
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
    app.discovery.records = peers.map((peerId, index) => ({ peerId, publicKey: index ? publicKey : 'f'.repeat(64), expiresAt: Date.now() / 1000 + 60, entries: [{ address: 'wss://directory.example/custom/socket', peer_id: peerId, relay: true }] }))
    app.discovery.rejected.add(peers[1])
    await app.render().connectCluster({ kind: 'cluster', clusterId: cluster, directories: ['wss://directory.example/custom/socket'] })
    const connection = app.render().ready
    assert.equal(connection?.entry.peer_id, peers[2])
    assert.equal(connection?.entry.local, undefined)
    assert.equal(connection?.principal.cluster_id, cluster)
    assert.deepEqual(app.calls.filter(call => call.startsWith('open:')), peers.map(peer => `open:${peer}`))
    assert.equal(app.calls.filter(call => call === 'subscribe').length, 1)
    assert.ok(app.stored.get(`vibeface.cluster-state.${cluster}`))
    assert.equal(JSON.parse(app.stored.get('vibeface.last-authenticated') ?? '{}').target.kind, 'cluster')
    app.sockets.at(-1)?.drop()
    assert.equal(app.render().phase, 'reconnecting')
    assert.equal(app.timers.size, 1)
    app.render().disconnect()
    assert.equal(app.timers.size, 0)
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
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, entries: [entry] }]
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
    app.discovery.records = [{ peerId: target, publicKey, expiresAt: Date.now() / 1000 + 60, entries: [entry] }]
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
  for (const source of sources) assert.equal(records.filter((record: { entries: ConnectionEntry[] }) => record.entries.some(entry => entry.address === source)).length, 2)
  assert.ok(records.every((record: { entries: ConnectionEntry[] }) => record.entries.length <= 2 && record.entries.every(entry => entry.relay && !entry.local && sources.includes(entry.address))))
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
    assert.ok(candidate.entries.length <= 2)
    for (const entry of candidate.entries) visited.add(entry.address)
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
