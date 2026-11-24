import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { offlineCopy } from '@vibeface/client/identity/copy.ts'
import type { Link } from '@vibeface/client/lib/link.ts'
import { unreachable } from '@vibeface/client/workspace/home.ts'
import { hookHarness, moduleLoader } from '../../../app/src/lib/test-harness.ts'
import type { Connection } from '../data/types.ts'

type Element = { type: unknown; key?: string; props: Record<string, unknown> }
const jsx = (type: unknown, props: Element['props'], key?: string): Element => ({ type, props, key })
const ui = new Proxy({}, { get: (_target, name) => String(name) })
function elements(node: unknown): Element[] {
  if (Array.isArray(node)) return node.flatMap(elements)
  if (!node || typeof node !== 'object' || !('props' in node)) return []
  const element = node as Element
  return [element, ...elements(element.props.children)]
}
function text(node: unknown): string {
  if (Array.isArray(node)) return node.map(text).join('')
  if (node && typeof node === 'object' && 'props' in node) return text((node as Element).props.children)
  return typeof node === 'string' || typeof node === 'number' ? String(node) : ''
}
function fixture() {
  const hooks = hookHarness(true)
  let retries = 0
  const ready = { principal: { network_id: 'network', device_id: 'phone', source: 'membership' }, entry: { kind: 'lan', device_id: 'computer' }, info: { name: '工作电脑' } }
  const device = { ready: ready as typeof ready | null, phase: 'ready', link: { kind: 'online', catchingUp: false } as Link, queued: null as number | null,
    entries: [], recovery: null, recovered: false, joining: null, entry: ready.entry, entryName: () => '工作电脑', retryNow() { retries++ }, directory: null }
  let connection: Connection
  const load = moduleLoader(createContext({}), (specifier) => {
    if (specifier === 'react') return hooks.react
    if (specifier === 'react/jsx-runtime') return { jsx, jsxs: jsx }
    if (specifier === '@vibeface/client/identity/context.tsx') return { useDevice: () => device }
    if (specifier === '@vibeface/client/identity/access.ts') return { accessOf }
    if (specifier === '@vibeface/client/identity/copy.ts') return { offlineCopy }
    if (specifier === '@vibeface/client/lib/node.ts') return { entryAddress: () => 'https://computer.example' }
    if (specifier === '@vibeface/client/workspace/labels.ts') return { entryLabel: () => '工作电脑' }
    if (specifier === '@vibeface/client/workspace/home.ts') return { unreachable }
    if (specifier === '@vibeface/client/workspace/context.tsx') return { WorkspaceProvider: 'SharedWorkspace' }
    if (specifier === '@vibeface/client/workspace/storage.ts') return { storageOf: () => ({ readStorage() {}, writeStorage() {} }) }
    if (specifier === '@/data/context') return { useWorkspace: () => ({ connection }) }
    if (specifier === '@/components/kit') return { toneText: { neutral: 'text-muted' } }
    if (specifier === '@/lib/utils') return { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }
    if (specifier === 'expo-router') return { useRouter: () => ({ push() {} }) }
    return ui
  }, filename => readFileSync(filename, 'utf8') + (filename.endsWith('/data/live.tsx') ? '\nexport { Device, useConnection }' : ''))
  const live = load(fileURLToPath(new URL('../data/live.tsx', import.meta.url)))
  const pill = load(fileURLToPath(new URL('../components/connection-pill.tsx', import.meta.url)))
  function call(component: unknown, props = {}) {
    assert.equal(typeof component, 'function')
    hooks.reset()
    return Reflect.apply(component as (props: object) => Element, undefined, [props])
  }
  function render() {
    const read = live.useConnection
    assert(typeof read === 'function')
    connection = Reflect.apply(read, undefined, []).connection
    return { connection, bar: call(pill.OfflineBar) }
  }
  return { device, ready, render, layout: () => call(live.Device), retries: () => retries }
}

test('native warm reconnect retains its workspace and shows recent records without an offline retry bar', () => {
  const app = fixture()
  const initial = app.layout()
  assert.equal(initial.type, 'SharedWorkspace')
  assert.equal(app.render().bar, null)
  app.device.phase = 'reconnecting'
  for (const link of [{ kind: 'offline', attempt: 1, retryInMs: 500, reason: 'closed' }, { kind: 'connecting', attempt: 1 }] satisfies Link[]) {
    app.device.link = link
    const retained = app.layout()
    assert.equal(retained.type, initial.type)
    assert.equal(retained.key, initial.key)
    assert.equal(retained.props.connection, app.ready)
    const { connection, bar } = app.render()
    assert.equal(connection.state, 'reconnecting')
    assert.equal(text(bar), '正在恢复连接，显示最近记录')
    assert.equal(bar.props.role, 'status')
    assert.equal(elements(bar).some(element => element.type === 'Button'), false)
  }
  app.device.queued = 2
  assert.equal(text(app.render().bar), '恢复连接中，排队第 2 位')
  app.device.link = { kind: 'offline', attempt: 2, retryInMs: 1000, reason: 'unreachable' }
  const failed = app.render()
  assert.equal(failed.connection.state, 'offline')
  const retry = elements(failed.bar).find(element => element.type === 'Button')?.props.onPress
  assert.equal(typeof retry, 'function')
  Reflect.apply(retry as () => void, undefined, [])
  assert.equal(app.retries(), 1)
  app.device.link = { kind: 'online', catchingUp: false }
  app.device.phase = 'ready'
  assert.equal(app.render().connection.state, 'online')
  assert.equal(app.render().bar, null)
})

test('native cold reconnect never claims that a live screen is being restored', () => {
  const app = fixture()
  app.device.ready = null
  app.device.phase = 'reconnecting'
  app.device.link = { kind: 'offline', attempt: 1, retryInMs: 500, reason: 'closed' }
  assert.equal(app.render().connection.state, 'offline')
  assert.notEqual(app.layout().type, 'SharedWorkspace')
})
