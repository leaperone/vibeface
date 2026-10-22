import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createContext, Script } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'

const network = 'b'.repeat(64), browser = 'c'.repeat(64), entry = 'a'.repeat(64)
const conversationId = 'conversation-1'
const bot = { bot_id: 'bot-1', conversation_id: conversationId, name: 'Bot 一', instructions: '', event_hash: 'h' }
const question = {
  question_id: 'q-1', conversation_id: conversationId, bot: null, severity: 'attention', text: '继续吗？', choices: [{ label: '继续' }],
  about: { node: entry, agent: 'codex', session_id: 's', cwd: '/p', summary: '' }, author: entry, created_at_ms: 1, answer: null,
}
const conversation = {
  conversation: { conversation_id: conversationId, title: '', owner: entry, owner_epoch: 1 }, messages: [], works: [], deliveries: [], scheduler: null,
  checks: [], local_node: entry, changes: [], bot_settings: { skills: [], schedules: [], memory: [] }, sent: [], questions: [question], sessions: [],
}
const notMember = 'device is not a member of this personal network'
const settle = () => new Promise(setImmediate)
const sourceRoot = fileURLToPath(new URL('../../', import.meta.url))
const nodeRequire = createRequire(import.meta.url)

/** Loads a TypeScript module the way the bundle sees it, with `stubs` for browser-only imports. */
function loader(globals: Record<string, unknown>, stubs: (specifier: string) => unknown) {
  const sandbox = createContext({ ...globals, URL, URLSearchParams, TextEncoder, Uint8Array, Error, JSON, Promise, crypto: globalThis.crypto })
  const modules = new Map<string, { exports: Record<string, unknown> }>()
  return function load(filename: string): Record<string, unknown> {
    const cached = modules.get(filename)
    if (cached) return cached.exports
    const module = { exports: {} as Record<string, unknown> }; modules.set(filename, module)
    const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
    const require = (specifier: string) => {
      const stub = stubs(specifier)
      if (stub) return stub
      if (!specifier.startsWith('.') && !specifier.startsWith('@/')) return nodeRequire(specifier)
      const path = specifier.startsWith('@/') ? resolve(sourceRoot, specifier.slice(2)) : resolve(dirname(filename), specifier)
      const file = extname(path) ? path : [`${path}.ts`, `${path}.tsx`].find(candidate => { try { readFileSync(candidate); return true } catch { return false } })!
      return load(file)
    }
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(module.exports, require, module)
    return module.exports
  }
}

/** The workspace state of one connected browser; React hooks are replayed by hand, one render per call. */
function workspace(source: 'membership' | 'local_session', readBefore: string[] | null) {
  const values: unknown[] = [], effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const afterRender: (() => void)[] = []
  const rpcs: string[] = [], stored = new Map<string, string>()
  if (readBefore) stored.set(`vibeface.read-before.${browser}.${network}`, JSON.stringify(readBefore))
  let cursor = 0
  const react = {
    createContext: () => ({ Provider: 'provider' }), useContext: () => null,
    useState(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = initial
      return [values[index], (next: unknown) => { values[index] = typeof next === 'function' ? (next as (value: unknown) => unknown)(values[index]) : next }]
    },
    useRef(initial: unknown) { const index = cursor++; if (!(index in values)) values[index] = { current: initial }; return values[index] },
    useEffect(run: () => (() => void) | void, dependencies: unknown[]) {
      const index = cursor++, previous = effects.get(index)
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) afterRender.push(() => {
        previous?.cleanup?.(); effects.set(index, { dependencies, cleanup: run() ?? undefined })
      })
    },
  }
  const writes = new Map<string, unknown>()
  const outbox = {
    all: async () => [...writes.values()], put: async (item: { key: string }) => { writes.set(item.key, structuredClone(item)) }, delete: async (key: string) => { writes.delete(key) },
  }
  const reads: Record<string, unknown> = {
    nodes: { nodes: [] }, bots: { bots: [bot], primary: conversationId }, questions: { questions: [question] }, conversation, open_works: { works: [] },
    notifications: { notifications: [{ notification_id: 'n-1', conversation_id: conversationId, kind: 'progress', text: '有新进展' }] },
  }
  const connection = {
    generation: 1, entry: { address: 'https://node.example', device_id: entry },
    principal: source === 'membership' ? { source, network_id: network, device_id: browser, epoch: 1, can_manage: false } : { source, network_id: network, device_id: browser, epoch: 1, can_manage: false },
    info: { device_id: entry, name: 'node', version: 'test', initialized: true, network_id: network },
    isCurrent: () => true, subscribe: () => () => {},
    async request(method: string) { if (method === 'write_state') return { state: 'writable', conflicts: [] }; return {} },
    async rpc(method: string, params: Record<string, unknown> = {}) {
      rpcs.push(method)
      if (method in reads) return reads[method]
      // The entry checks the signature against the membership directory, as `AuthorizationView::member` does.
      if (source !== 'membership') throw Object.assign(new Error(notMember), { responded: true })
      if (method === 'ack') { reads.notifications = { notifications: [] }; return {} }
      if (method === 'answer') return { status: 'answered', message: { message_id: `a-${params.question_id}`, text: params.text } }
      return {}
    },
  }
  const timers = { setTimeout: () => 0, clearTimeout() {}, setInterval: () => 0, clearInterval() {} }
  const load = loader({
    window: { ...timers, location: { href: `https://node.example/b/${bot.bot_id}` }, localStorage: { getItem: (key: string) => stored.get(key) ?? null, setItem: (key: string, value: string) => stored.set(key, value) } },
    document: { hidden: false, addEventListener() {}, removeEventListener() {} },
    location: { pathname: `/b/${bot.bot_id}`, search: '' },
    ...timers,
  }, (specifier) => {
    if (specifier === 'react') return react
    if (specifier === 'react/jsx-runtime') return { jsx: (_type: unknown, props: unknown) => props }
    if (specifier.endsWith('/transport.js')) return { signClient: async () => 'signature', validTarget: (value: string) => value, validEndpoint: (value: string) => value }
    if (specifier.endsWith('/vibeface_secure.js')) return { attestationPayload: () => new Uint8Array() }
    if (specifier === '@/lib/outbox') return { ...load(resolve(sourceRoot, 'lib/outbox.ts')), indexedOutbox: () => outbox }
    return undefined
  })
  const Provider = load(fileURLToPath(new URL('./context.tsx', import.meta.url))).WorkspaceProvider as (props: Record<string, unknown>) => { value: Record<string, any> }
  const tabs = { leader: () => true, post() {}, listen: () => () => {} }
  const render = () => {
    cursor = 0
    const { value } = Provider({ connection, tabs, openConversation() {}, children: null })
    afterRender.splice(0).forEach(run => run())
    return value
  }
  return { render, rpcs, writes, stored, close() { for (const effect of effects.values()) effect.cleanup?.() } }
}

async function open(source: 'membership' | 'local_session', readBefore: string[] | null = []) {
  const app = workspace(source, readBefore)
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
  assert.match(html, /<button[^>]*>.*用加入链接加入<\/button>/)
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
    assert.equal(app.rpcs.filter(method => method === 'ack').length, 1, 'read once, then the notification is gone')
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
    assert.equal(app.stored.get(`vibeface.read-before.${browser}.${network}`), JSON.stringify(['n-1']))
  } finally { app.close() }
})
