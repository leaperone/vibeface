import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext, Script } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'
import { createHash } from 'node:crypto'
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
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 3 }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 0 }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1 }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2 }), 'saved')
})

const target = 'a'.repeat(64), cluster = 'b'.repeat(64), subject = 'c'.repeat(64), publicKey = 'd'.repeat(64)
const entry: ConnectionEntry = { address: 'https://node.example', peer_id: target, relay: true }
const genesis = { version: 2, cluster_id: cluster, owner_public_key: publicKey }
const membership = { cluster_id: cluster, members: [], revoked: [], heads: [], epoch: 1 }
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
        if (holds.has(method)) return holds.get(method)?.promise
        if (method === 'info') return { peer_id: candidate.peer_id, public_key: publicKey, version: 'test', roles: ['relay'], initialized: !bootstrap }
        if (method === 'authenticate' && local) return { principal: { cluster_id: cluster } }
        if (method === 'local_access') { localState.enabled = false; return { enabled: false } }
        if (method === 'pair_status') return { status: 'granted' }
        if (method === 'prepare_genesis') {
          const genesis = { version: 2, cluster_id: cluster, owner_public_key: publicKey, signature: '', initial_members: [{ subject_id: target, public_key: publicKey, kind: 'node', name: 'This device', node_roles: ['relay'], manage: false }, { ...review.new_member, manage: true }] }
          return { state: { genesis, checkpoint: null, events: [] }, signing: { domain: 'identity-genesis', payload: canonical(genesis) } }
        }
        return {}
      } }
    },
  }
  let verifications = 0, released = 0
  const secure = {
    verifyAuthentication: () => { verifications++; return { auth: { local_auto_access: true }, principal: { source: local ? 'local_session' : 'membership', cluster_id: cluster, subject_id: subject, epoch: 1, can_manage: false }, view: membership } },
    verifiedView: () => membership,
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
  return { render, calls, holds, sockets, stored, timers, location, localState, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
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
    assert.match(app.render().error, /无法恢复原集群授权/)
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
