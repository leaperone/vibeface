import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'

type Element = { type: unknown; props: Record<string, any> }
const settle = () => new Promise(setImmediate)
function pending() {
  let resolve!: (value?: any) => void, reject!: (cause: Error) => void
  const promise = new Promise<any>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
function component(file: string, dependencies: Record<string, unknown>) {
  const hooks = hookHarness(true)
  const jsx = (type: unknown, props: Element['props']) => ({ type, props })
  const ui = new Proxy({}, { get: (_target, name) => String(name) })
  const load = moduleLoader(createContext({ window: { addEventListener() {}, removeEventListener() {} }, setTimeout, clearTimeout }), (specifier) => {
    if (specifier === 'react') return hooks.react
    if (specifier === 'react/jsx-runtime') return { jsx, jsxs: jsx }
    return dependencies[specifier] ?? ui
  })
  const module = load(fileURLToPath(new URL(file, import.meta.url))) as Record<string, (props: Record<string, unknown>) => Element>
  return {
    render(name: string, props: Record<string, unknown> = {}) {
      hooks.reset()
      const output = module[name](props)
      hooks.afterRender.splice(0).forEach(effect => effect())
      return output
    },
    close() { hooks.effects.forEach(effect => effect.cleanup?.()) },
  }
}
function elements(node: unknown, visibleOnly = false): Element[] {
  if (Array.isArray(node)) return node.flatMap(child => elements(child, visibleOnly))
  if (!node || typeof node !== 'object' || !('props' in node)) return []
  const element = node as Element
  if (visibleOnly && element.props.hidden) return []
  return [element, ...elements(element.props.children, visibleOnly)]
}
const terms = new Proxy({}, { get: (_target, name) => ({ label: String(name), icon: 'Icon' }) })
const cn = (...values: unknown[]) => values.filter(Boolean).join(' ')

test('device choices wait for directory, nodes and basic AI settings, then stay visible through refresh', async () => {
  const directory = pending()
  const ready = { entry: { device_id: 'computer' }, principal: { can_manage: true }, isCurrent: () => true }
  const device = { ready, identity: { deviceId: 'browser' }, directory: { members: [{ device_id: 'browser', kind: 'client', name: 'Browser', manage: true }], revoked: [] }, refreshDirectory: () => directory.promise }
  const workspace = { nodesRead: false, synced: false, nodes: [] as any[], error: null as Error | null, connectionOnline: true }
  const app = component('./devices.tsx', {
    '@/lib/glossary': { terms }, '@/lib/utils': { cn },
    '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
    '@vibeface/client/lib/node.ts': { entryAddress: () => 'http://localhost' },
    '@vibeface/client/workspace/labels.ts': { deviceForm: () => 'browser', deviceOnline: () => true, isLoopback: () => true },
    '@vibeface/client/workspace/model.ts': { botReadiness: () => null },
  })
  const render = () => app.render('DevicesPage')
  const visible = () => elements(render(), true)
  try {
    assert.equal(render().props['aria-busy'], true)
    assert.equal(visible().some(element => element.type === 'Link'), false, 'the directory alone must not paint an incomplete real list')
    directory.resolve(); await settle()
    assert.equal(render().props['aria-busy'], true)
    workspace.nodes = [{ device_id: 'computer', name: 'Computer' }]
    const ai = elements(render()).find(element => element.type === 'AiSettings')
    assert.ok(ai)
    assert.equal(visible().some(element => element.type === 'Link'), false, 'mount settings to read them without moving visible choices')
    ai.props.onInitialRead()
    assert.equal(render().props['aria-busy'], true, 'retained nodes from a previous connection cannot stand in for this connection’s first read')
    workspace.nodesRead = true
    assert.equal(render().props['aria-busy'], false)
    assert.equal(visible().filter(element => element.type === 'Link').length, 2)
    workspace.synced = false; workspace.nodesRead = false; workspace.nodes = []
    assert.equal(render().props['aria-busy'], false, 'a later refresh does not replace available controls with loading')
  } finally { app.close() }
})

test('directory and workspace read failures reveal the signed directory and retry', async () => {
  const ready = { entry: { device_id: 'computer' }, principal: { can_manage: false }, isCurrent: () => true }
  let directoryReads = 0
  const app = component('./devices.tsx', {
    '@/lib/glossary': { terms }, '@/lib/utils': { cn },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready, identity: { deviceId: 'browser' }, directory: { members: [{ device_id: 'browser', kind: 'client', name: 'Browser' }], revoked: [] }, refreshDirectory: async () => { directoryReads++; throw new Error('directory unavailable') } }) },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ synced: false, nodes: [], error: new Error('nodes unavailable'), connectionOnline: true }) },
    '@vibeface/client/lib/node.ts': { entryAddress: () => 'http://localhost' },
    '@vibeface/client/identity/copy.ts': { addOnManager: () => '管理设备可添加设备' },
    '@vibeface/client/workspace/labels.ts': { deviceForm: () => 'browser', deviceOnline: () => true, isLoopback: () => true },
    '@vibeface/client/workspace/model.ts': { botReadiness: () => null },
  })
  try {
    app.render('DevicesPage'); await settle()
    const output = app.render('DevicesPage'), visible = elements(output, true)
    assert.equal(output.props['aria-busy'], false)
    assert.equal(visible.filter(element => element.type === 'Link').length, 1)
    const error = visible.find(element => element.type === 'InlineError')
    assert.match(error?.props.error.message, /directory unavailable/)
    error?.props.action.onClick(); await settle()
    assert.equal(directoryReads, 2)
  } finally { app.close() }
})

test('connected editable devices reveal recovery without an AI callback when offline or a workspace read fails', async t => {
  for (const outcome of ['offline', 'error', 'online'] as const) await t.test(outcome, async () => {
    const directory = pending()
    const ready = { entry: { device_id: 'computer' }, principal: { can_manage: true }, isCurrent: () => outcome !== 'offline' }
    const app = component('./devices.tsx', {
      '@/lib/glossary': { terms }, '@/lib/utils': { cn },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready, identity: { deviceId: 'browser' }, directory: { members: [{ device_id: 'browser', kind: 'client', name: 'Browser', manage: true }], revoked: [] }, refreshDirectory: () => directory.promise }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ nodesRead: outcome === 'online', nodes: [{ device_id: 'computer', name: 'Computer' }], error: outcome === 'error' ? new Error('workspace unavailable') : null, connectionOnline: outcome !== 'offline' }) },
      '@vibeface/client/lib/node.ts': { entryAddress: () => 'http://localhost' },
      '@vibeface/client/workspace/labels.ts': { deviceForm: () => 'browser', deviceOnline: () => outcome !== 'offline', isLoopback: () => true },
      '@vibeface/client/workspace/model.ts': { botReadiness: () => null },
    })
    const render = () => app.render('DevicesPage')
    try {
      assert.equal(render().props['aria-busy'], true, 'the initial directory read still has to settle')
      directory.resolve(); await settle()
      const output = render()
      assert.equal(output.props['aria-busy'], outcome === 'online')
      if (outcome === 'online') {
        assert.equal(elements(output, true).some(element => element.type === 'AiSettings'), false, 'a healthy current connection still waits for basic AI settings')
        elements(output).find(element => element.type === 'AiSettings')?.props.onInitialRead()
      }
      const visible = elements(render(), true)
      assert.equal(render().props['aria-busy'], false)
      assert.equal(visible.filter(element => element.type === 'Link').length, 2)
      assert.equal(visible.some(element => element.type === 'AiSettings'), true, 'AI errors and their retry actions remain in the visible content')
    } finally { app.close() }
  })
})

test('AI first-read notification settles on success or failure, ignores an obsolete connection and does not wait for provider discovery', async t => {
  for (const outcome of ['success', 'failure', 'obsolete', 'unmounted'] as const) await t.test(outcome, async () => {
    const settings = pending(), discovery = pending()
    let current = true, notifications = 0
    const calls: string[] = []
    const ready = { isCurrent: () => current, request: (method: string) => { calls.push(method); return method === 'node_settings' ? settings.promise : discovery.promise } }
    const app = component('./ai-settings.tsx', {
      '@/lib/glossary': { terms }, '@/lib/utils': { cn },
      '@vibeface/client/identity/copy.ts': { deviceCopy: { appServer: {}, roles: { bot: {}, execution: {} } } },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ isCurrent: () => true, refresh: async () => {} }) },
      '@vibeface/client/lib/node.ts': { parseNodeSettings: (value: unknown) => value, providerGroups: () => ({ shown: [] }), folderSuggestions: () => [] },
    })
    const render = () => app.render('AiSettings', { compact: true, onInitialRead: () => { notifications++ } })
    try {
      render()
      assert.equal(notifications, 0)
      if (outcome === 'obsolete') current = false
      if (outcome === 'unmounted') app.close()
      if (outcome === 'failure') settings.reject(new Error('settings unavailable'))
      else settings.resolve({ providers: [{ provider: 'cursor', roles: [], probe: { catalog: { state: 'unavailable' } } }], settings: { roots: [], manager: null } })
      await settle()
      assert.equal(notifications, outcome === 'obsolete' || outcome === 'unmounted' ? 0 : 1)
      const output = render()
      if (outcome === 'failure') {
        assert.equal(output.type, 'InlineError')
        assert.equal(output.props.action.label, '重试')
      }
      if (outcome === 'success') {
        assert.deepEqual(calls, ['node_settings', 'configure_provider'])
        assert.equal(notifications, 1, 'provider discovery remains pending after the settings gate opens')
      }
    } finally { app.close(); discovery.resolve() }
  })
})
