import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import type { Link } from '@vibeface/client/lib/link.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'

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
  let connected = true, retries = 0
  const ready = { principal: { network_id: 'network', device_id: 'browser' }, entry: { device_id: 'computer' }, info: { name: '工作电脑' } }
  const device = { ready: ready as typeof ready | null, phase: 'ready', link: { kind: 'online', catchingUp: false } as Link, queued: null as number | null,
    entries: [], tabs: {}, initialInvitation: '', joining: null, retryNow() { retries++ }, disconnect() {}, directory: null }
  const workspace = { get connectionOnline() { return connected }, nodes: [] }
  const dependencies: Record<string, unknown> = {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    '@vibeface/client/identity/access.ts': { accessOf },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace, WorkspaceProvider: 'WorkspaceProvider' },
    '@vibeface/client/workspace/labels.ts': { nodeName: () => '工作电脑' },
    '@/features/identity/onboarding-flag': { useOnboarding: () => null },
    '@/lib/glossary': { terms: new Proxy({}, { get: (_target, name) => ({ label: String(name), icon: 'Icon' }) }) },
    '@tanstack/react-router': { createFileRoute: () => () => ({}), useNavigate: () => () => {}, useRouterState: ({ select }: { select: (state: unknown) => unknown }) => select({ location: { pathname: '/b/bot', href: '/b/bot' } }) },
  }
  const load = moduleLoader(createContext({}), (specifier) => {
    if (specifier === 'react') return { ...hooks.react, lazy: () => 'Onboarding' }
    if (specifier === 'react/jsx-runtime') return { jsx, jsxs: jsx }
    if (specifier === '@/lib/utils') return { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') }
    return dependencies[specifier] ?? ui
  }, filename => readFileSync(filename, 'utf8') + (filename.endsWith('/routes/_app.tsx') ? '\nexport { AppLayout }' : ''))
  const shell = load(fileURLToPath(new URL('./workspace-shell.tsx', import.meta.url)))
  const route = load(fileURLToPath(new URL('../../routes/_app.tsx', import.meta.url)))
  function render(component: unknown) {
    assert.equal(typeof component, 'function')
    hooks.reset()
    return Reflect.apply(component as () => Element, undefined, [])
  }
  return { device, ready, render: () => render(shell.ConnectionNotice), layout: () => render(route.AppLayout), retries: () => retries,
    connection(value: boolean) { connected = value } }
}

test('warm reconnect keeps the workspace and gives quiet, honest status before a retry fails', () => {
  const app = fixture()
  const initial = app.layout()
  assert.equal(initial.type, 'WorkspaceProvider')
  assert.equal(app.render(), null)
  app.connection(false)
  app.device.phase = 'reconnecting'
  for (const link of [{ kind: 'offline', attempt: 1, retryInMs: 500, reason: 'closed' }, { kind: 'connecting', attempt: 1 }] satisfies Link[]) {
    app.device.link = link
    const retained = app.layout()
    assert.equal(retained.type, 'WorkspaceProvider')
    assert.equal(retained.key, initial.key)
    assert.equal(retained.props.connection, app.ready)
    const notice = app.render()
    assert.equal(text(notice), '正在恢复连接，显示最近记录')
    assert.equal(elements(notice).some(element => element.type === 'Button'), false)
    assert.equal(notice.props.role, 'status')
  }
  app.device.queued = 3
  assert.equal(text(app.render()), '恢复连接中，排队第 3 位')
  app.device.link = { kind: 'offline', attempt: 2, retryInMs: 1000, reason: 'unreachable' }
  const failed = app.render()
  assert.equal(text(failed), '连不上工作电脑重试连接')
  const retry = elements(failed).find(element => element.type === 'Button')?.props.onClick
  assert.equal(typeof retry, 'function')
  Reflect.apply(retry as () => void, undefined, [])
  assert.equal(app.retries(), 1)
  app.connection(true)
  assert.equal(app.render(), null)
})

test('cold reconnect retains the initial connection screen instead of pretending cached content is available', () => {
  const app = fixture()
  app.device.ready = null
  app.device.phase = 'reconnecting'
  app.device.link = { kind: 'offline', attempt: 1, retryInMs: 500, reason: 'closed' }
  assert.equal(app.layout().type, 'ShellSkeleton')
})
