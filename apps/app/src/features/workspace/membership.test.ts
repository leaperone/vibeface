import { completeHistory } from '@vibeface/client/workspace/history-fixture.ts'
import assert from 'node:assert/strict'
import test from 'node:test'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'

const network = 'b'.repeat(64), browser = 'c'.repeat(64), entry = 'a'.repeat(64)
const conversationId = 'conversation-1'
const bot = { bot_id: 'bot-1', conversation_id: conversationId, name: 'Bot 一', instructions: '', models: {}, skills: null, event_hash: 'h' }
const target = { kind: 'bot_main' as const, bot_id: bot.bot_id }
const question = {
  question_id: 'q-1', conversation_id: conversationId, target, bot: { bot_id: bot.bot_id, name: bot.name }, severity: 'attention', text: '继续吗？', choices: [{ label: '继续' }],
  about: { node: entry, agent: 'codex', session_id: 's', cwd: '/p', summary: '' }, author: entry, created_at_ms: 1, answer: null, closed: null,
}
const conversation = {
  history_authority: "authority", history: completeHistory(),
  conversation: { conversation_id: conversationId, title: '', owner: entry, owner_epoch: 1 }, messages: [], works: [], deliveries: [], scheduler: null,
  checks: [], local_node: entry, changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, questions: [question], sessions: [],
}
const notMember = 'device is not a member of this personal network'
const settle = () => new Promise(setImmediate)
const sourceRoot = fileURLToPath(new URL('../../', import.meta.url))
const clientRoot = fileURLToPath(new URL('../../../../../packages/client/src/', import.meta.url))
const nodeRequire = createRequire(import.meta.url)

/** Loads a TypeScript module the way the bundle sees it, with `stubs` for browser-only imports. */
function loader(globals: Record<string, unknown>, stubs: (specifier: string) => unknown) {
  const sandbox = createContext({ ...globals, URL, URLSearchParams, TextEncoder, TextDecoder, Uint8Array, Error, JSON, Promise, crypto: globalThis.crypto })
  const load = moduleLoader(sandbox, (specifier, filename) => {
    const stub = stubs(specifier)
    if (stub) return stub
    if (specifier.startsWith('@vibeface/client/')) return load(resolve(clientRoot, specifier.slice(17)))
    if (specifier.startsWith('@vibeface/uiarc/')) return load(resolve(clientRoot, '../../uiarc/src', specifier.slice('@vibeface/uiarc/'.length)))
    if (specifier.endsWith('.module.css')) return { default: new Proxy({}, { get: (_target, key) => String(key) }) }
    if (!specifier.startsWith('.') && !specifier.startsWith('@/')) return createRequire(filename)(specifier)
    const path = specifier.startsWith('@/') ? resolve(sourceRoot, specifier.slice(2)) : resolve(dirname(filename), specifier)
    const file = extname(path) ? path : [`${path}.ts`, `${path}.tsx`].find(candidate => { try { readFileSync(candidate); return true } catch { return false } })!
    return load(file)
  })
  return load
}

/** The workspace state of one connected browser; React hooks are replayed by hand, one render per call. */
function workspace(source: 'membership' | 'local_session', readBefore: string[] | null, initialConversation: Record<string, unknown> = conversation) {
  const hooks = hookHarness(true), { effects, afterRender } = hooks
  const rpcs: string[] = [], stored = new Map<string, string>(), failing = new Set<string>(), ticks = new Set<() => void>()
  const foreground = new Set<() => void>(), delayed = new Set<() => void>()
  let visible = true, now = Date.now(), sendsValidators = true
  let ackGate: Promise<void> | null = null, localGate: Promise<void> | null = null
  const listeners = new Set<(event: unknown) => void>()
  const snapshots: { requested: unknown; kind: string; responseBytes: number; requestBytes: number; fields: string[]; removed: string[]; submittedMessages: number }[] = []
  const receipts = new Map<string, { fields: Record<string, string>; messages?: string[]; signature: string }>()
  let invalidReply: 'validator' | 'delta-base' | 'missing-field' | 'row-index' | 'row-hash' | null = null
  let notificationReads = [
    { notification_id: 'n-1', conversation_id: conversationId, target, kind: 'progress', text: '有新进展' },
    { notification_id: 'n-session', conversation_id: conversationId, target, kind: 'session_attributed', text: 'tests fail' },
  ]
  if (readBefore) stored.set(`vibeface.read-before.${browser}.${network}`, JSON.stringify(readBefore))
  const react = { ...hooks.react, createContext: () => ({ Provider: 'provider' }) }
  const writes = new Map<string, unknown>()
  const outbox = {
    all: async () => { if (localGate) await localGate; return [...writes.values()] }, put: async (item: { key: string }) => { writes.set(item.key, structuredClone(item)) }, delete: async (key: string) => { writes.delete(key) },
  }
  const reads: Record<string, unknown> = {
    write_state: { state: 'writable', conflicts: [] }, nodes: { nodes: [] }, bots: { bots: [bot], primary: conversationId }, questions: { questions: [question] }, conversation: initialConversation, open_works: { works: [] },
    notifications: { notifications: notificationReads },
    workspace_state: { channels: [], tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false } },
  }
  let connection = {
    generation: 1, entry: { address: 'https://node.example', device_id: entry },
    principal: { source, network_id: network, device_id: browser, epoch: 1, can_manage: false },
    info: { device_id: entry, name: 'node', version: 'test', initialized: true, network_id: network },
    isCurrent: () => true, subscribe(listener: (event: unknown) => void) { listeners.add(listener); return () => listeners.delete(listener) },
    async request(method: string) { rpcs.push(method); return reads[method] ?? {} },
    async rpc(method: string, params: Record<string, unknown> = {}) {
      rpcs.push(method)
      if (failing.has(method)) throw new Error('entry unreachable')
      if (method === 'conversation') {
        const payload = await reads.conversation
        assert.ok(payload && typeof payload === 'object')
        if (!sendsValidators) return payload
        const hash = (value: unknown) => createHash('sha256').update(JSON.stringify(value)).digest('hex')
        const validator = hash(payload)
        const hashes = Object.fromEntries(Object.entries(payload).map(([key, value]) => [key, hash(value)]))
        const messages = 'messages' in payload && Array.isArray(payload.messages) ? payload.messages : null
        const receipt = { fields: hashes, signature: '1'.repeat(128), ...(messages && messages.length <= 128 ? { messages: messages.map(hash) } : {}) }
        const previous = typeof params.if_snapshot === 'string' ? receipts.get(params.if_snapshot) : null
        let reply: Record<string, unknown> = { ...payload, snapshot_id: validator, snapshot_receipt: receipt }
        let kind = 'full', submittedMessages = messages?.length ?? 0, fields: Record<string, unknown> = {}, removed: string[] = []
        if (previous) {
          assert.deepEqual(structuredClone(params.snapshot_receipt), previous)
          if (params.if_snapshot === validator) {
            kind = 'unchanged'; submittedMessages = 0
            reply = { unchanged: true, snapshot_id: invalidReply === 'validator' ? '0'.repeat(64) : validator }
          } else {
            kind = 'delta'
            fields = Object.fromEntries(Object.entries(payload).filter(([key]) => hashes[key] !== previous.fields[key]))
            removed = Object.keys(previous.fields).filter(key => !(key in hashes))
            const previousRows = previous.messages
            let messageDelta: { rows: unknown[] } | undefined
            submittedMessages = 0
            if (messages && receipt.messages && previousRows && 'messages' in fields) {
              const rows = messages.map(value => {
                const from = previousRows.indexOf(hash(value))
                if (from >= 0) return { previous: invalidReply === 'row-index' ? previousRows.length : invalidReply === 'row-hash' ? (from + 1) % previousRows.length : from }
                submittedMessages++
                return { value }
              })
              messageDelta = { rows }
              delete fields.messages
            }
            reply = { snapshot_delta: true, base_snapshot_id: invalidReply === 'delta-base' ? '0'.repeat(64) : params.if_snapshot,
              snapshot_id: validator, fields: invalidReply === 'missing-field' ? {} : fields, removed, snapshot_receipt: receipt, ...(messageDelta && { messages_delta: messageDelta }) }
          }
        }
        receipts.set(validator, receipt)
        snapshots.push({ requested: params.if_snapshot, kind, submittedMessages, fields: Object.keys(fields), removed,
          requestBytes: Buffer.byteLength(JSON.stringify(params)), responseBytes: Buffer.byteLength(JSON.stringify(reply)) })
        return reply
      }
      if (method in reads) return reads[method]
      // The entry checks the signature against the membership directory, as `AuthorizationView::member` does.
      if (source !== 'membership') throw Object.assign(new Error(notMember), { responded: true })
      if (method === 'ack') {
        if (ackGate) await ackGate
        notificationReads = notificationReads.filter((item) => item.notification_id !== String(params.notification_id))
        reads.notifications = { notifications: notificationReads }
        return {}
      }
      if (method === 'answer') return { status: 'answered', message: { message_id: `a-${params.question_id}`, text: params.text } }
      return {}
    },
  }
  const timers = { setTimeout: (tick: () => void) => { delayed.add(tick); return tick }, clearTimeout(tick: () => void) { delayed.delete(tick) }, setInterval: (tick: () => void) => { ticks.add(tick); return tick }, clearInterval(tick: () => void) { ticks.delete(tick) } }
  const load = loader({
    location: { pathname: `/b/${bot.bot_id}`, search: '' },
    Date: class extends Date { static now() { return now } }, ...timers,
  }, (specifier) => {
    if (specifier === 'react') return react
    if (specifier === 'react/jsx-runtime') return { jsx: (_type: unknown, props: unknown) => props }
    return undefined
  })
  const platform = {
    reserveDeliveryIntent: async () => { throw new Error('unexpected delivery reservation') },
    secure: {
      attestationPayload: (network: string, scope: string, body: string) => new TextEncoder().encode(JSON.stringify({ network, scope, body })),
      verify(device: string, domain: string, payload: Uint8Array, signature: string) {
        assert.equal(device, browser)
        assert.equal(domain, 'attestation')
        assert.equal(signature, createHash('sha512').update(payload).digest('hex'))
      },
    },
    transport: { signClient: async (domain: string, payload: Uint8Array) => { assert.equal(domain, 'attestation'); return createHash('sha512').update(payload).digest('hex') } },
    storage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) }, outbox, self: '这个浏览器',
    notifier: { name: '浏览器提醒', permission: async () => 'default', request: async () => 'default', async show() {} },
    onForeground(listener: () => void) { foreground.add(listener); return () => foreground.delete(listener) }, foreground: () => visible,
  }
  const Provider = load(resolve(clientRoot, 'workspace/context.tsx')).WorkspaceProvider as (props: Record<string, unknown>) => { value: Record<string, any> }
  const tabs = { leader: () => true, post() {}, listen: () => () => {} }
  const render = () => {
    hooks.reset()
    const { value } = Provider({ platform, connection, tabs, initialBot: bot.bot_id, openConversation() {}, children: null })
    afterRender.splice(0).forEach(run => run())
    return value
  }
  return { render, rpcs, writes, stored, reads, failing, ticks, snapshots,
    withoutValidators() { sendsValidators = false },
    holdLocal() {
      let release: () => void = () => {}
      localGate = new Promise<void>(resolve => { release = resolve })
      return release
    },
    holdAck() {
      let release: () => void = () => {}
      ackGate = new Promise<void>(resolve => { release = resolve })
      return release
    },
    addNotification(id: string) {
      notificationReads = [...notificationReads, { ...notificationReads[0], notification_id: id, kind: 'progress' }]
      reads.notifications = { notifications: notificationReads }
    },
    background() { visible = false },
    advance(ms: number) { now += ms },
    foreground() { visible = true; for (const listener of foreground) listener() },
    runTimers() { for (const tick of [...delayed]) { delayed.delete(tick); tick() } },
    push(event: unknown) { for (const listener of listeners) listener(event) },
    disconnect() { connection.isCurrent = () => false },
    reconnect(node = entry, epoch = connection.principal.epoch) {
      connection = { ...connection, generation: connection.generation + 1, entry: { ...connection.entry, device_id: node },
        principal: { ...connection.principal, epoch }, info: { ...connection.info, device_id: node }, isCurrent: () => true }
    },
    invalidateReply(kind: typeof invalidReply = 'validator') { invalidReply = kind },
    close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

async function open(source: 'membership' | 'local_session', readBefore: string[] | null = [], initialConversation: Record<string, unknown> = conversation) {
  const app = workspace(source, readBefore, initialConversation)
  for (let i = 0; i < 6; i++) { app.render(); await settle() }
  app.render().setView('chat')
  for (let i = 0; i < 3; i++) { app.render(); await settle() }
  return app
}

test('a browser in a local session is shown as 本机管理, sees the workspace read-only and is told to join', async () => {
  const app = await open('local_session')
  try {
    const view = app.render()
    assert.equal(view.bots.length, 1, 'the records still show')
    assert.equal(view.writable, false, 'every write control is off')
    assert.equal(view.joined, false)
    await assert.rejects(view.answer(question, '继续'), /本机管理不能发送/)
    assert.equal(app.rpcs.includes('answer'), false, 'nothing is sent to be refused')
  } finally { app.close() }
  const load = loader({}, () => undefined)
  const { LocalSession } = load(fileURLToPath(new URL('./local-session.tsx', import.meta.url))) as { LocalSession: (props: { onJoin(): void }) => unknown }
  const { createElement } = nodeRequire('react'), { renderToStaticMarkup } = nodeRequire('react-dom/server')
  const html: string = renderToStaticMarkup(createElement(LocalSession, { onJoin() {} }))
  assert.match(html, /本机管理/)
  assert.doesNotMatch(html, /还没加入/)
  assert.match(html, /<button[^>]*>/)
  assert.match(html, /加入/)
})

test('a browser that is not a member never sends a read receipt, so nothing retries on every refresh', async () => {
  const app = await open('local_session')
  try {
    for (let tick = 0; tick < 3; tick++) { await app.render().refresh(); await settle() }
    assert.equal(app.rpcs.filter(method => method === 'ack').length, 0)
    assert.equal(app.render().error, null, 'no failure is shown for a write it never made')
    assert.equal(app.writes.size, 0)
  } finally { app.close() }
})

test('a member reads its notifications and can answer', async () => {
  const app = await open('membership')
  try {
    assert.equal(app.render().writable, true)
    for (let tick = 0; tick < 3; tick++) { await app.render().refresh(); await settle() }
    assert.equal(app.rpcs.filter(method => method === 'ack').length, 1, 'read once, then ordinary notification is gone')
    assert.deepEqual(app.render().notifications.map((item: { notification_id: string }) => item.notification_id), ['n-session'], 'a session reminder stays until its reminder row is opened')
    assert.equal((await app.render().answer(question, '继续')).status, 'answered')
    assert.equal(app.render().joined, true)
  } finally { app.close() }
})

test('a new device starts with the history read and sends no read receipts for it', async () => {
  const app = await open('membership', null)
  try {
    for (let tick = 0; tick < 3; tick++) { await app.render().refresh(); await settle() }
    assert.deepEqual(app.render().notifications, [])
    assert.equal(app.rpcs.filter(method => method === 'ack').length, 0)
    assert.equal(app.stored.get(`vibeface.read-before.${browser}.${network}`), JSON.stringify(['n-1', 'n-session']))
  } finally { app.close() }
})

test('a failed refresh keeps the chat on screen and pauses writes until a read succeeds', async () => {
  const app = await open('membership')
  try {
    assert.equal(app.render().data?.conversation?.conversation_id, conversationId)
    app.failing.add('nodes')
    for (let tick = 0; tick < 3; tick++) await assert.rejects(app.render().refresh(), /entry unreachable/)
    const stale = app.render()
    assert.equal(stale.data?.conversation?.conversation_id, conversationId, 'the chat does not blank on a failed read')
    assert.equal(stale.writable, false, 'writes wait for a read that succeeds')
    app.failing.delete('nodes')
    await app.render().refresh(); await settle()
    assert.equal(app.render().writable, true)
  } finally { app.close() }
})

test('while a computer is offline, the periodic check rereads which devices are online', async () => {
  const app = await open('membership')
  try {
    const computer = { device_id: entry, name: 'node', revoked: false, local: false, online: false, capabilities: null }
    const nodeReads = () => app.rpcs.filter(method => method === 'nodes').length
    const idle = nodeReads()
    for (const tick of app.ticks) tick()
    await settle()
    assert.equal(nodeReads(), idle, 'nothing is reread while no device is offline')
    app.reads.nodes = { nodes: [computer] }
    await app.render().refresh(); await settle(); app.render()
    const before = nodeReads()
    app.reads.nodes = { nodes: [{ ...computer, online: true }] }
    for (const tick of app.ticks) tick()
    await settle(); app.render()
    assert.equal(nodeReads(), before + 1)
    assert.equal(app.render().nodes[0]?.online, true, 'the computer shows online without waiting for the full reread')
  } finally { app.close() }
})

test('a healthy foreground keeps pushed records without a workspace or conversation read', async () => {
  const app = await open('membership')
  try {
    app.runTimers(); await settle(); app.render()
    app.rpcs.length = 0
    app.push({ type: 'records', value: { conversation_id: conversationId, history_authority: 'authority', refresh_metadata: false, works: [],
      messages: [{ role: 'user', text: 'background push', event_hash: 'message-hash', message_id: 'pushed', created_at_ms: 1 }] } })
    await settle()
    app.foreground(); app.runTimers(); await settle()
    assert.equal(app.render().data.messages[0]?.text, 'background push')
    assert.deepEqual(app.rpcs, [])
  } finally { app.close() }
})

test('a new connection confirms the chat once and reads current write permissions before enabling actions', async (t) => {
  const base = { ...conversation, scheduler: { jobs: [], unanswered: ['old'], scope_jobs: [] },
    messages: [{ role: 'user', text: 'x'.repeat(32768), message_id: 'm', event_hash: 'h', created_at_ms: 1 }] }
  const app = await open('membership', [], base)
  try {
    assert.equal(app.snapshots.length, 1, 'one cold conversation read')
    const cold = app.snapshots[0]
    app.foreground(); await settle()
    app.disconnect()
    assert.equal(app.render().writable, false)
    app.rpcs.length = 0
    app.reconnect()
    assert.equal(app.render().writable, false)
    await settle()
    const resumed = app.render()
    assert.equal(resumed.writable, true)
    assert.equal(resumed.data.conversation.conversation_id, conversationId)
    assert.deepEqual([...app.rpcs].sort(), ['bots', 'conversation', 'nodes', 'notifications', 'open_works', 'questions', 'workspace_state', 'write_state'])
    assert.equal(app.snapshots.length, 2)
    const unchanged = app.snapshots[1]
    assert.equal(unchanged.kind, 'unchanged')
    const { scheduler, ...remaining } = base
    assert.deepEqual([...resumed.data.scheduler.unanswered], scheduler.unanswered)
    app.reads.conversation = { ...remaining, conversation: { ...conversation.conversation, title: 'while offline' } }
    app.reads.write_state = { state: 'read_only', conflicts: [] }
    app.disconnect(); app.reconnect(); app.render(); await settle()
    const changed = app.render()
    const delta = app.snapshots[2]
    assert.equal(delta.kind, 'delta')
    assert.deepEqual(delta.fields, ['conversation'])
    assert.deepEqual(delta.removed, ['scheduler'])
    assert.equal(changed.data.scheduler, null)
    assert.equal(changed.data.messages[0].text, base.messages[0].text)
    for (const compact of [unchanged, delta]) assert.ok(compact.requestBytes + compact.responseBytes < cold.responseBytes / 4)
    t.diagnostic(`conversation request+response bytes: cold ${cold.requestBytes}+${cold.responseBytes}, unchanged ${unchanged.requestBytes}+${unchanged.responseBytes}, changed ${delta.requestBytes}+${delta.responseBytes}`)
    assert.equal(changed.data.conversation.title, 'while offline')
    assert.equal(changed.writable, false)
  } finally { app.close() }
})

test('an invalid validator falls back to a full read and a different entry starts with a full read', async () => {
  const app = await open('membership')
  try {
    app.invalidateReply()
    app.disconnect(); app.reconnect(); app.render(); await settle()
    assert.equal(app.render().writable, true)
    assert.deepEqual(app.snapshots.map(read => read.kind), ['full', 'unchanged', 'full'])
    assert.equal(app.snapshots.at(-1)?.requested, undefined)
    app.disconnect(); app.reconnect('d'.repeat(64)); app.render(); await settle()
    assert.equal(app.render().writable, true)
    assert.equal(app.snapshots.at(-1)?.requested, undefined)
    app.disconnect(); app.reconnect('d'.repeat(64), 2); app.render(); await settle()
    assert.equal(app.snapshots.at(-1)?.requested, undefined, 'a changed membership epoch requires a fresh base')
  } finally { app.close() }
})

for (const failure of ['delta-base', 'missing-field'] as const) test(`a changed conversation with an invalid ${failure} requires a full read`, async () => {
  const app = await open('membership')
  try {
    app.invalidateReply(failure)
    app.reads.conversation = { ...conversation, questions: [] }
    app.disconnect(); app.reconnect(); app.render(); await settle()
    assert.deepEqual(app.snapshots.map(read => read.kind), ['full', 'delta', 'full'])
    assert.equal(app.render().data.questions.length, 0)
    assert.equal(app.render().writable, true)
  } finally { app.close() }
})

test('an unverified conversation stays visible with writes paused through a failed reconnect', async () => {
  const app = await open('membership')
  try {
    app.push({ type: 'records', value: { conversation_id: conversationId, history_authority: 'authority', refresh_metadata: false, works: [],
      messages: [{ role: 'user', text: 'display only', event_hash: 'pushed-hash', message_id: 'pushed', created_at_ms: 1 }] } })
    await settle()
    app.disconnect(); app.failing.add('conversation'); app.reconnect(); app.render(); await settle()
    const unavailable = app.render()
    assert.equal(unavailable.data.messages[0].text, 'display only')
    assert.equal(unavailable.writable, false)
    assert.equal(unavailable.dataOrigin, 'cache')
    await assert.rejects(unavailable.answer(question, '继续'))
    assert.ok(!app.rpcs.includes('answer'))
    app.failing.clear()
    await unavailable.refresh()
    assert.equal(app.render().data.messages.length, 0, 'the node confirmed the unmerged original snapshot')
    assert.equal(app.render().writable, true)
  } finally { app.close() }
})

const transcriptRow = (number: number) => ({
  role: 'user', text: `message-${number} ` + 'x'.repeat(2048), message_id: `message-${number}`,
  event_hash: `hash-${number}`, created_at_ms: number, rank: String(number).padStart(5, '0'),
})

test('a warm append and a rolling transcript window transfer only new or replaced rows', async (t) => {
  const messages = Array.from({ length: 16 }, (_, index) => transcriptRow(index))
  const app = await open('membership', [], { ...conversation, messages })
  try {
    const appended = [...messages, transcriptRow(16)]
    app.reads.conversation = { ...conversation, messages: appended }
    app.disconnect(); app.reconnect(); app.render(); await settle()
    const first = app.render()
    assert.equal(first.writable, true)
    assert.deepEqual([...first.data.messages].map((row: { text: string }) => row.text), appended.map(row => row.text))
    const append = app.snapshots.at(-1)!
    assert.equal(append.kind, 'delta')
    assert.equal(append.submittedMessages, 1)
    assert.ok(!append.fields.includes('messages'))
    const updated = { ...appended[1], event_hash: 'replaced', text: 'edited in place' }
    const rolled = [updated, ...appended.slice(2), transcriptRow(17)]
    app.reads.conversation = { ...conversation, messages: rolled }
    app.disconnect(); app.reconnect(); app.render(); await settle()
    const resumed = app.render()
    const roll = app.snapshots.at(-1)!
    assert.equal(roll.kind, 'delta')
    assert.equal(roll.submittedMessages, 2)
    assert.deepEqual([...resumed.data.messages].map((row: { text: string }) => row.text), rolled.map(row => row.text))
    const cold = app.snapshots[0]
    for (const compact of [append, roll]) assert.ok(compact.requestBytes + compact.responseBytes < cold.responseBytes / 3)
    t.diagnostic(`transcript request+response bytes: cold ${cold.requestBytes}+${cold.responseBytes}, append ${append.requestBytes}+${append.responseBytes}, roll and edit ${roll.requestBytes}+${roll.responseBytes}`)
  } finally { app.close() }
})

for (const defect of ['row-index', 'row-hash'] as const) test(`an invalid ${defect} cannot authorize a warm transcript`, async () => {
  const previous = [transcriptRow(1), transcriptRow(2)], messages = [...previous, transcriptRow(3)]
  const app = await open('membership', [], { ...conversation, messages: previous })
  try {
    app.invalidateReply(defect)
    app.reads.conversation = { ...conversation, messages }
    app.disconnect(); app.reconnect(); app.render(); await settle()
    assert.deepEqual(app.snapshots.map(read => read.kind), ['full', 'delta', 'full'])
    const view = app.render()
    assert.deepEqual([...view.data.messages].map((row: { text: string }) => row.text), messages.map(row => row.text))
    assert.equal(view.writable, true)
  } finally { app.close() }
})

test('hidden maintenance stays idle and a missed five-minute check runs once on return', async () => {
  const app = await open('membership')
  try {
    app.reads.nodes = { nodes: [{ device_id: entry, name: 'node', revoked: false, local: false, online: false, capabilities: null }] }
    app.runTimers(); await settle(); app.render()
    app.rpcs.length = 0
    app.background()
    app.advance(300_001)
    for (const tick of app.ticks) tick()
    app.runTimers(); await settle()
    assert.deepEqual(app.rpcs, [], 'no node polling or periodic full read while hidden')
    app.advance(300_001)
    app.foreground(); app.foreground()
    app.failing.add('nodes'); app.runTimers(); await settle()
    assert.equal(app.rpcs.filter(method => method === 'nodes').length, 1)
    assert.equal(app.render().writable, false)
    app.failing.clear()
    app.foreground(); app.runTimers(); await settle()
    assert.equal(app.rpcs.filter(method => method === 'nodes').length, 2)
    assert.equal(app.render().writable, true)
    app.rpcs.length = 0
    app.background(); app.advance(299_999); app.foreground(); app.runTimers(); await settle()
    assert.deepEqual(app.rpcs, [])
  } finally { app.close() }
})

test('a Web conversation from a node without validators survives only a reconnect to the same scope', async () => {
  const app = workspace('membership', [], { ...conversation, messages: [transcriptRow(1)] })
  try {
    app.withoutValidators()
    app.render(); await settle()
    assert.equal(app.render().data.messages[0].message_id, 'message-1')
    assert.equal(app.snapshots.length, 0)
    app.disconnect(); app.failing.add('conversation'); app.reconnect(); app.render(); await settle()
    const retained = app.render()
    assert.equal(retained.data.messages[0].message_id, 'message-1')
    assert.equal(retained.dataOrigin, 'cache')
    assert.equal(retained.writable, false)
    app.disconnect(); app.reconnect(entry, 2)
    assert.equal(app.render().data, null, 'the first render for another membership epoch hides the previous display')
    await settle()
    assert.equal(app.render().data, null)
  } finally { app.close() }
})

test('Web acknowledges the first confirmed open chat and a reconnected chat once during concurrent foreground events', async () => {
  const app = workspace('membership', [])
  let release = app.holdAck()
  try {
    app.render().setView('chat')
    await settle()
    const ackCount = () => app.rpcs.filter(method => method === 'ack').length
    assert.equal(ackCount(), 1, 'initial hydration sends the receipt without waiting for a timer')
    app.foreground(); app.foreground(); await settle()
    assert.equal(ackCount(), 1)
    release(); await settle()
    assert.deepEqual(app.render().notifications.map((note: { notification_id: string }) => note.notification_id), ['n-session'])
    app.disconnect(); app.addNotification('n-reconnected')
    release = app.holdAck()
    app.reconnect(); app.render(); await settle()
    assert.equal(ackCount(), 2, 'the new connection has its own confirmed open chat')
    app.foreground(); app.foreground(); await settle()
    assert.equal(ackCount(), 2)
    release(); await settle()
    assert.deepEqual(app.render().notifications.map((note: { notification_id: string }) => note.notification_id), ['n-session'])
  } finally { release(); app.close() }
})

for (const localFinishesLast of [false, true]) test(`a successful metadata retry reconciles an unknown dismissal once, local ledger finishes ${localFinishesLast ? 'last' : 'first'}`, async () => {
  const app = await open('membership')
  let release = () => {}
  try {
    app.render().setView('other')
    app.failing.add('dismiss_question')
    await assert.rejects(app.render().dismissQuestion(question), /entry unreachable/)
    assert.equal(app.writes.size, 1)
    app.failing.clear()
    app.failing.add('nodes')
    app.disconnect()
    if (localFinishesLast) release = app.holdLocal()
    app.reconnect(); app.render(); await settle()
    assert.equal(app.render().writable, false)
    assert.equal(app.writes.size, 1)
    app.failing.clear()
    app.reads.questions = { questions: [] }
    app.rpcs.length = 0
    await app.render().refresh(); await settle()
    if (localFinishesLast) {
      assert.equal(app.writes.size, 1)
      release(); await settle()
    }
    assert.equal(app.writes.size, 0)
    assert.equal(app.rpcs.filter(method => method === 'questions').length, 2, 'the retry read and the pending dismissal reconciliation')
    assert.ok(!app.rpcs.includes('dismiss_question'), 'the original unknown dismissal was already reflected by the node')
    await app.render().refresh(); await settle()
    assert.equal(app.rpcs.filter(method => method === 'questions').length, 3, 'later refreshes only read metadata')
  } finally { release(); app.close() }
})
