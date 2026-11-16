import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { hookHarness, moduleLoader } from '../../../app/src/lib/test-harness.ts'
import * as node from '../../../../packages/client/src/lib/node.ts'
import * as model from '../../../../packages/client/src/workspace/model.ts'
import type { nodeSettingsResource, triageSettingsResource, useSettingsResource } from '../features/devices/settings-resource.ts'

type NodeResource = ReturnType<typeof useSettingsResource<ReturnType<typeof nodeSettingsResource.parse>, keyof typeof nodeSettingsResource.strategies>>
type TriageResource = ReturnType<typeof useSettingsResource<ReturnType<typeof triageSettingsResource.parse>, keyof typeof triageSettingsResource.strategies>>
function operationTypes(node: NodeResource, triage: TriageResource) {
  // @ts-expect-error A triage operation cannot update the node settings resource.
  void node.call('test', 'test_webhook')
  // @ts-expect-error Node configuration requires its settings parameters.
  void node.call('roots', 'configure_node')
  // @ts-expect-error Provider IDs come from the existing ProviderChange type.
  void node.call('provider', 'configure_provider', { provider: 'unknown' })
  // @ts-expect-error A webhook test does not accept arbitrary RPC parameters.
  void triage.call('test', 'test_webhook', { secret: 'extra' })
  // @ts-expect-error A token revocation requires the token identifier.
  void triage.call('revoke', 'revoke_decision_token', { label: 'chat' })
}
void operationTypes

const nodeSnapshot = {
  settings: { roots: ['/projects'], manager: null }, defaults: { workspace: '/projects' },
  readiness: { manager: null }, providers: [], display_paths: { '/projects': '~/projects' }, suggested_roots: [],
}
const triageSnapshot = {
  default_outlet: 'both', projects: [], webhook_url: 'https://example.test/hook', scan_codex_sessions: false, codex_sessions: null,
  webhook_secret_set: true, decision_url: 'https://example.test/decision', device_id: 'device', decision_tokens: [],
  outbox: { pending: 0, delivered: 0, dead: 0 },
  status: { budget: { day: '2026-11-16', used: 0, limit: 10, retry_at_ms: 1794873600000, paused_reason: null }, pending_events: 0, inflight_events: 0, unknown_events: 0 },
}
const tick = () => new Promise<void>(resolve => setImmediate(resolve))
function deferred() {
  let resolve = (_value: unknown) => {}, reject = (_reason: unknown) => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
function fixture(resource: 'nodeSettingsResource' | 'triageSettingsResource') {
  const hooks = hookHarness()
  const requests: ({ method: string; params: Record<string, unknown> | undefined } & ReturnType<typeof deferred>)[] = []
  let current = true, writes = 0
  const connection = {
    isCurrent: () => current,
    request(method: string, params?: Record<string, unknown>) {
      const request = { method, params, ...deferred() }
      requests.push(request)
      return request.promise
    },
  }
  let ready: typeof connection | null = connection
  const load = moduleLoader(createContext({}), specifier => {
    if (specifier === 'react') return {
      ...hooks.react,
      useState(initial: unknown) {
        const [value, set] = hooks.react.useState(initial)
        assert(typeof set === 'function')
        return [value, (next: unknown) => { writes++; Reflect.apply(set, undefined, [next]) }]
      },
    }
    if (specifier === '@vibeface/client/identity/context.tsx') return { useDevice: () => ({ ready }) }
    if (specifier === '@vibeface/client/lib/node.ts') return node
    if (specifier === '@vibeface/client/workspace/model.ts') return model
    throw new Error(`Unexpected dependency: ${specifier}`)
  })
  const loaded = load(fileURLToPath(new URL('../features/devices/settings-resource.ts', import.meta.url)))
  const loadedHook = loaded.useSettingsResource
  assert(typeof loadedHook === 'function')
  const hook = loadedHook
  function render() {
    hooks.reset()
    const result: unknown = Reflect.apply(hook, undefined, [loaded[resource]])
    assert(node.isRecord(result))
    assert(typeof result.busy === 'string')
    assert(result.error === null || node.isRecord(result.error))
    if (result.error !== null) assert(typeof result.error.at === 'string' && typeof result.error.text === 'string')
    assert(typeof result.call === 'function' && typeof result.load === 'function')
    const call = result.call, reload = result.load
    for (const effect of hooks.afterRender.splice(0)) effect()
    return {
      value: result.value, busy: result.busy, error: result.error === null ? null : { at: result.error.at, text: result.error.text },
      async call(key: string, method: string, params?: Record<string, unknown>, at?: string): Promise<unknown> {
        return await Reflect.apply(call, undefined, [key, method, params, at])
      },
      async load(): Promise<unknown> { return await Reflect.apply(reload, undefined, []) },
    }
  }
  return {
    render, requests,
    trace: () => requests.map(({ method, params }) => ({ method, params })),
    writes: () => writes,
    stale() { current = false },
    disconnect() { current = false; ready = null; render() },
    unmount() { for (const effect of hooks.effects.values()) effect.cleanup?.() },
  }
}

test('node reads and configuration responses set the snapshot without another read; probes reload once', async () => {
  const f = fixture('nodeSettingsResource')
  assert.equal(f.render().value, null)
  f.requests[0].resolve(nodeSnapshot)
  await tick()
  assert.deepEqual(f.render().value, nodeSnapshot)
  assert.deepEqual(f.trace(), [{ method: 'node_settings', params: undefined }])

  const provider = { ...nodeSnapshot, suggested_roots: ['/next'] }
  const change = f.render().call('codex:bot', 'configure_provider', { provider: 'codex', bot: true }, 'codex')
  assert.equal(f.render().busy, 'codex:bot')
  f.requests[1].resolve(provider)
  assert.equal(await change, provider)
  assert.deepEqual(f.render().value, provider)

  const configured = { ...nodeSnapshot, settings: { roots: ['/next'], manager: null } }
  const save = f.render().call('roots', 'configure_node', { roots: ['/next'], manager: null }, 'roots')
  f.requests[2].resolve(configured)
  assert.equal(await save, configured)
  assert.deepEqual(f.render().value, configured)

  const probed = { provider: 'codex', checked_at_ms: 123 }
  const probe = f.render().call('codex:probe', 'provider_probe', { provider: 'codex' }, 'codex')
  f.requests[3].resolve(probed)
  await tick()
  assert.equal(f.render().busy, 'codex:probe')
  f.requests[4].resolve(nodeSnapshot)
  assert.equal(await probe, probed)
  assert.deepEqual(f.render().value, nodeSnapshot)
  assert.equal(f.render().busy, '')
  assert.deepEqual(f.trace(), [
    { method: 'node_settings', params: undefined },
    { method: 'configure_provider', params: { provider: 'codex', bot: true } },
    { method: 'configure_node', params: { roots: ['/next'], manager: null } },
    { method: 'provider_probe', params: { provider: 'codex' } },
    { method: 'node_settings', params: undefined },
  ])
  f.unmount()
})

test('triage configuration uses its snapshot; webhook and token operations keep their outputs and each reload once', async () => {
  const f = fixture('triageSettingsResource')
  f.render()
  f.requests[0].resolve(triageSnapshot)
  await tick()
  const params = { default_outlet: 'record', projects: [], webhook_url: null, scan_codex_sessions: false, codex_sessions: null, webhook_secret: null }
  const configured = { ...triageSnapshot, default_outlet: 'record', webhook_url: null, webhook_secret_set: false }
  const save = f.render().call('webhook', 'configure_triage', params)
  f.requests[1].resolve(configured)
  assert.equal(await save, configured)
  assert.deepEqual(f.render().value, { ...configured, webhook_url: '' })
  assert.equal(f.requests.length, 2)

  const outputs = [
    { method: 'test_webhook', params: undefined, key: 'test', result: { ok: false, status: 503, error: 'unavailable' } },
    { method: 'create_decision_token', params: { label: 'chat' }, key: 'token', result: { token_id: 'token-id', token: 'one-time-token', label: 'chat', created_ms: 123 } },
    { method: 'revoke_decision_token', params: { token_id: 'token-id' }, key: 'revoke', result: { revoked: true } },
  ]
  for (const operation of outputs) {
    const index: number = f.requests.length
    const work = f.render().call(operation.key, operation.method, operation.params)
    f.requests[index].resolve(operation.result)
    await tick()
    assert.equal(f.requests.length, index + 2)
    assert.equal(f.render().busy, operation.key)
    f.requests[index + 1].resolve(triageSnapshot)
    assert.equal(await work, operation.result)
    assert.deepEqual(f.render().value, triageSnapshot)
  }
  assert.deepEqual(f.trace(), [
    { method: 'triage_settings', params: undefined },
    { method: 'configure_triage', params },
    { method: 'test_webhook', params: undefined }, { method: 'triage_settings', params: undefined },
    { method: 'create_decision_token', params: { label: 'chat' } }, { method: 'triage_settings', params: undefined },
    { method: 'revoke_decision_token', params: { token_id: 'token-id' } }, { method: 'triage_settings', params: undefined },
  ])
  f.unmount()
})

test('an unknown write result reports its keyed error without a read or retry; a later explicit load can reconcile', async () => {
  const f = fixture('nodeSettingsResource')
  f.render(); f.requests[0].resolve(nodeSnapshot); await tick()
  const work = f.render().call('roots', 'configure_node', { roots: ['/next'], manager: null }, 'roots')
  f.requests[1].reject(new Error('结果待确认。'))
  assert.equal(await work, null)
  assert.deepEqual(f.render().error, { at: 'roots', text: '结果待确认。' })
  assert.deepEqual(f.render().value, nodeSnapshot)
  assert.equal(f.requests.length, 2)
  const reload = f.render().load()
  assert.equal(f.render().error, null)
  f.requests[2].resolve(nodeSnapshot)
  assert.equal(await reload, nodeSnapshot)
  assert.deepEqual(f.trace(), [
    { method: 'node_settings', params: undefined },
    { method: 'configure_node', params: { roots: ['/next'], manager: null } },
    { method: 'node_settings', params: undefined },
  ])
  f.unmount()
})

test('a failed follow-up read preserves the previous snapshot and discards the operation result without resending it', async () => {
  const f = fixture('triageSettingsResource')
  f.render(); f.requests[0].resolve(triageSnapshot); await tick()
  const work = f.render().call('token', 'create_decision_token', { label: 'chat' }, 'token')
  f.requests[1].resolve({ token: 'one-time-token' }); await tick()
  f.requests[2].reject(new Error('读取失败。'))
  assert.equal(await work, null)
  assert.deepEqual(f.render().value, triageSnapshot)
  assert.deepEqual(f.render().error, { at: 'token', text: '读取失败。' })
  assert.equal(f.render().busy, '')
  assert.equal(f.requests.length, 3)
  f.unmount()
})

test('a newer call owns state and stale completions cannot start a reload or replace keyed errors', async () => {
  const f = fixture('nodeSettingsResource')
  f.render(); f.requests[0].resolve(nodeSnapshot); await tick()
  const old = f.render().call('codex:probe', 'provider_probe', { provider: 'codex' }, 'codex')
  const latest = f.render().call('roots', 'configure_node', { roots: ['/next'], manager: null }, 'roots')
  const writes = f.writes()
  f.requests[1].resolve({ provider: 'codex' })
  assert.equal(await old, null)
  assert.equal(f.writes(), writes)
  assert.equal(f.requests.length, 3)
  assert.equal(f.render().busy, 'roots')
  f.requests[2].reject(new Error('最新保存失败。'))
  assert.equal(await latest, null)
  assert.deepEqual(f.render().error, { at: 'roots', text: '最新保存失败。' })
  f.unmount()
})

test('a newer call also invalidates an in-flight reload and suppresses an older rejection', async () => {
  const f = fixture('nodeSettingsResource')
  f.render(); f.requests[0].resolve(nodeSnapshot); await tick()
  const probe = f.render().call('codex:probe', 'provider_probe', { provider: 'codex' }, 'codex')
  f.requests[1].resolve({ provider: 'codex' }); await tick()
  const latest = f.render().call('roots', 'configure_node', { roots: ['/next'], manager: null }, 'roots')
  const configured = { ...nodeSnapshot, settings: { roots: ['/next'], manager: null } }
  f.requests[3].resolve(configured)
  await latest
  const beforeReload = f.writes()
  f.requests[2].resolve(nodeSnapshot)
  assert.equal(await probe, null)
  assert.equal(f.writes(), beforeReload)
  assert.deepEqual(f.render().value, configured)

  const old = f.render().call('roots', 'configure_node', { roots: ['/old'], manager: null }, 'roots')
  const fresh = f.render().load()
  f.requests[5].resolve(configured)
  await fresh
  const beforeRejection = f.writes()
  f.requests[4].reject(new Error('旧请求失败。'))
  assert.equal(await old, null)
  assert.equal(f.writes(), beforeRejection)
  assert.equal(f.render().error, null)
  assert.deepEqual(f.render().value, configured)
  assert.equal(f.requests.length, 6)
  f.unmount()
})

test('connection invalidation, disconnect and unmount discard pending answers before and after a reload', async () => {
  for (const stop of ['stale', 'disconnect', 'unmount'] as const) for (const afterReload of [false, true]) {
    const f = fixture('triageSettingsResource')
    f.render(); f.requests[0].resolve(triageSnapshot); await tick()
    const work = f.render().call('test', 'test_webhook')
    if (afterReload) { f.requests[1].resolve({ ok: true, status: 200, error: null }); await tick() }
    f[stop]()
    f.requests[afterReload ? 2 : 1].resolve(afterReload ? triageSnapshot : { ok: true, status: 200, error: null })
    assert.equal(await work, null)
    assert.equal(f.requests.length, afterReload ? 3 : 2)
    assert.deepEqual(f.render().value, stop === 'disconnect' ? null : triageSnapshot)
    if (stop !== 'unmount') assert.equal(await f.render().call('test', 'test_webhook'), null)
    f.unmount()
  }
})
