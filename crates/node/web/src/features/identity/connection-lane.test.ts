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

const target = 'a'.repeat(64), network = 'b'.repeat(64), subject = 'c'.repeat(64), publicKey = 'd'.repeat(64)
const entry: ConnectionEntry = { address: 'https://node.example', node_id: target, relay: true }
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

function deviceHarness({ pairing = false, bootstrap = false, saved = false } = {}) {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const afterRender: (() => void)[] = [], timers = new Map<number, () => void>()
  const stored = new Map<string, string>(), calls: string[] = []
  const holds = new Map<string, ReturnType<typeof deferred>>()
  const sockets: { entry: ConnectionEntry; close(): void; drop(): void }[] = []
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
    initClient: async () => ({ nodeId: subject, publicKey }), readInvitation: JSON.parse, signClient: () => 'signature',
    async connectDevice(candidate: ConnectionEntry, options: { onClose(cause: Error): void; signal: AbortSignal }) {
      calls.push(`open:${candidate.node_id}`)
      let closed = false
      const close = () => { if (closed) return; closed = true; options.onClose(new Error('connection lost')) }
      sockets.push({ entry: candidate, close, drop: close })
      options.signal.addEventListener('abort', close)
      if (holds.has('open')) await holds.get('open')?.promise
      return { nodeId: candidate.node_id, publicKey, close, async request(method: string) {
        calls.push(method)
        if (holds.has(method)) return holds.get(method)?.promise
        if (method === 'info') return { node_id: candidate.node_id, public_key: publicKey, version: 'test', roles: ['relay'], initialized: !bootstrap }
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
    verifyAuthentication: () => { verifications++; return { principal: { network_id: network, subject_id: subject, epoch: 1, can_manage: false }, view: membership } },
    verifiedView: () => membership,
    openPackage: async () => ({ networkId: network, epoch: 1, payload: { genesis }, root: { publicKeyHex: () => publicKey, sign: () => 'signature' }, release() { released++ } }),
    Identity: { generate: () => ({ nodeId: () => network, publicKeyHex: () => publicKey, secretHex: () => 'private-test-key', sign: () => 'signature', free() { released++ } }) },
    sealPackage: () => new Uint8Array([1, 2, 3]), downloadPackage() {}, verifyState: () => ({ free() {}, viewJson: () => JSON.stringify(membership) }),
  }
  const timer = (run: () => void) => { const id = ++nextTimer; timers.set(id, run); return id }
  const sandbox = createContext({
    localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) },
    location: { hash: '', href: 'https://node.example' }, history: { replaceState() {} }, URL, URLSearchParams, AbortController, TextEncoder, Uint8Array, Error, crypto: globalThis.crypto,
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
  return { render, calls, holds, sockets, stored, timers, verifications: () => verifications, released: () => released, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

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
    await app.render().pair(JSON.stringify({ url: entry.address, node_id: target, relay: true, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
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
      await app.render().pair(JSON.stringify({ url: entry.address, node_id: target, relay: true, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
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
      const challenge = { network_id: network, owner_public_key: publicKey, subject_id: subject, target_node_id: target, challenge_id: 'challenge', genesis_hash: createHash('sha256').update(canonical(genesis)).digest('hex'), expires_at_ms: Date.now() + 60000, nonce: 'e'.repeat(64) }
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
    assert.equal(app.render().ready?.entry.node_id, target)
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
    assert.equal(app.render().ready?.entry.node_id, target)
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
    await app.render().pair(JSON.stringify({ url: entry.address, node_id: target, relay: true, capability: 'local-test-capability' }), 'browser', 'test-only-passphrase')
    app.render().downloadBackup()
    await app.render().confirmBootstrap(true)
    assert.equal(app.render().ready?.entry.node_id, target)
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
    const replacement = { ...entry, node_id: 'f'.repeat(64) }
    await app.render().connect(replacement)
    hold.reject(Object.assign(new Error('old permission rejected'), { responded: true }))
    await settle()
    assert.equal(app.render().ready?.entry.node_id, replacement.node_id)
    assert.equal(app.render().ready?.isCurrent(), true)
    assert.equal(app.render().phase, 'ready')
    assert.equal(app.render().error, '')
  } finally { app.close() }
})

for (const stage of ['open', 'authenticate']) test(`switching devices during reconnect ${stage} invalidates the old retry`, async () => {
  const app = deviceHarness({ saved: true })
  try {
    app.render(); await settle(); app.render()
    assert.equal(app.render().ready?.entry.node_id, target)
    app.sockets[0].drop(); app.sockets[0].drop()
    assert.equal(app.timers.size, 1)
    const hold = deferred(); app.holds.set(stage, hold)
    const [id, retry] = [...app.timers.entries()][0]
    app.timers.delete(id); retry()
    await settle()
    assert.equal(app.calls.filter(call => call === `open:${target}`).length, 2)
    app.render().disconnect()
    app.holds.delete(stage)
    const replacement = { ...entry, node_id: 'f'.repeat(64) }
    await app.render().connect(replacement)
    hold.resolve({})
    await settle()
    assert.equal(app.render().ready?.entry.node_id, replacement.node_id)
    assert.equal(app.render().phase, 'ready')
    assert.equal(app.render().error, '')
    assert.equal(app.timers.size, 0)
    assert.equal(app.calls.filter(call => call === `open:${target}`).length, 2)
    assert.equal(app.calls.filter(call => ['bootstrap', 'pair_request', 'rpc'].includes(call)).length, 0)
  } finally { app.close() }
})
