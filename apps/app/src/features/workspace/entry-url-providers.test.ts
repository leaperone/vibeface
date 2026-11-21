
import assert from "node:assert/strict"
import test from "node:test"
import * as nodeSettings from "@vibeface/client/lib/node.ts"

import type { NodeSettingsResponse, ProviderReadiness, ProviderStatus } from "@vibeface/client/lib/node.ts"

import { deviceCopy, waitingCopy } from "@vibeface/client/identity/copy.ts"
import { accessOf } from "@vibeface/client/identity/access.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"


import { componentHarness, pendingResult, elementText, settleComponent } from './entry-url-test-kit.test-support.ts'


test("anonymous workspace navigation preserves the requested page without redirecting twice", async (t) => {
  for (const [requested, resolved] of [
    ['/', undefined],
    ['/settings', undefined],
    ['/b/bot?conversation=chat#latest', undefined],
    ['/settings', '/join'],
    ['/b/bot?conversation=chat#latest', '/connect'],
    ['/settings', '/settings'],
  ] satisfies [string, string | undefined][]) await t.test(`${requested} after ${resolved ?? 'initial load'}`, () => {
    const location = (href: string) => ({ href, pathname: new URL(href, 'https://app.vibeface.dev').pathname })
    const state = { location: location(requested), resolvedLocation: resolved ? location(resolved) : undefined }
    const app = componentHarness('../../routes/_app.tsx', {
      '@tanstack/react-router': {
        createFileRoute: () => () => ({}), Navigate: 'Navigate',
        useNavigate: () => () => {},
        useRouterState: ({ select }: { select: (value: typeof state) => unknown }) => select(state),
      },
      '@vibeface/client/identity/access.ts': { accessOf },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({
        ready: null, phase: 'disconnected', entries: [], initialInvitation: '', joining: false, link: { kind: 'idle' },
      }) },
      '@/features/identity/onboarding-flag': { useOnboarding: () => null },
    }, {}, 'AppLayout')
    try {
      const navigation = app.render('AppLayout').filter(({ type }) => type === 'Navigate')
      assert.equal(navigation.length, 1)
      assert.equal(navigation[0]?.props.to, '/connect')
      assert.equal(navigation[0]?.props.replace, true)
      const search: Record<string, string> = requested === '/' ? {} : { redirect: requested }
      assert.equal(JSON.stringify(navigation[0]?.props.search), JSON.stringify(search))
      state.location = location('/connect?' + new URLSearchParams(search))
      for (let render = 0; render < 3; render++) {
        assert.equal(app.render('AppLayout').filter(({ type }) => type === 'Navigate').length, 0,
          'the pending connect page must not become another return target')
      }
    } finally { app.close() }
  })
})


test('opening AI settings connects only Cursor and retains focus until it can recheck CLI login', async (t) => {
  for (const outcome of ['returned', 'read-back', 'unreadable', 'connection-changed', 'recheck-failed', 'same-device-reconnected']) await t.test(outcome, async () => {
    const unreadable = outcome === 'unreadable' || outcome === 'same-device-reconnected'
    const provider: ProviderStatus = {
      provider: 'cursor', authentication_source: 'local_cli_login', roles: [], supports: ['agent', 'execution'], bin: '/fixture/cursor-sdk-bridge', app_server: null,
      endpoint: null, execution_model: '', execution_readiness: { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } }, setup: null, installed: false, home: null, sessions: null, env: [],
      state: 'off', issues: [], readiness: { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } },
      probe: { provider: 'cursor', config_id: 'fixture', checked_at_ms: null, launch: { state: 'unchecked' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
      observed: { installed: { status: 'rejected', evidence: 'binary_discovery' }, launchable: { status: 'unknown', evidence: 'launch_prerequisites' }, authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
    }
    const otherProvider: ProviderStatus = { ...provider, provider: 'codex', authentication_source: 'private_login', roles: ['execution'], probe: { ...provider.probe, provider: 'codex' } }
    const initial: NodeSettingsResponse = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: null }, providers: [provider, otherProvider], display_paths: {}, suggested_roots: [] }
    const signedOut: NodeSettingsResponse = { ...initial, providers: [{ ...provider, bin: '/fixture/agent', installed: true,
      setup: { directory: null, command: "'/fixture/agent' login" }, probe: { ...provider.probe, checked_at_ms: 1, authentication: 'sign_in_required' } }, otherProvider] }
    const signedIn: NodeSettingsResponse = { ...signedOut, providers: signedOut.providers.map((item) => item.provider === 'cursor' ? { ...item, probe: { ...item.probe, checked_at_ms: 2, authentication: 'authenticated' } } : item) }
    const first = pendingResult(), second = pendingResult(), readback = pendingResult(), replacementRead = pendingResult(), calls: { method: string; params: unknown }[] = []
    let current = true, reads = 0
    const connection = { entry: { device_id: 'fixture-node' }, isCurrent: () => current, request: async (method: string, params?: unknown) => {
      calls.push({ method, params })
      if (method === 'node_settings') return ++reads === 2 && unreadable ? readback.promise : signedOut
      return calls.filter((call) => call.method === 'configure_provider').length === 1 ? first.promise : second.promise
    } }
    const replacementCalls: { method: string; params: unknown }[] = []
    const replacement = { entry: connection.entry, isCurrent: () => !current, request: async (method: string, params?: unknown) => {
      replacementCalls.push({ method, params })
      if (method === 'node_settings' && outcome === 'same-device-reconnected') return replacementRead.promise
      return signedIn
    } }
    let ready = connection
    let focused = () => {}
    const app = componentHarness('../devices/ai-settings.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => ({ isCurrent: () => true, refresh: async () => {} }) },
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'Tasks' } } },
    }, { window: { addEventListener(_event: string, callback: () => void) { focused = callback }, removeEventListener() { focused = () => {} } } })
    const render = () => app.render('AiSettings', { compact: true })
    try {
      render(); await settleComponent(); render(); render()
      const writes = () => calls.filter((call) => call.method === 'configure_provider')
      assert.equal(writes().length, 1, 'entering settings discovers Cursor once without clicking automatic connection')
      const cached = render().find(({ props }) => props.item !== undefined)
      assert.equal(cached?.props.busy, 'connect_local')
      assert.equal(nodeSettings.parseProvider(cached?.props.item).probe.authentication, 'sign_in_required', 'the CLI login command is already available during the old probe')
      focused(); focused()
      assert.equal(writes().length, 1, 'focus cannot duplicate an in-flight connection write')
      if (outcome === 'connection-changed') {
        current = false; ready = replacement
        render(); await settleComponent(); render(); render()
        first.resolve(signedOut)
        for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
        assert.equal(writes().length, 1, 'an old connection never consumes its pending focus')
        assert.deepEqual(JSON.parse(JSON.stringify(replacementCalls)), [
          { method: 'node_settings' }, { method: 'configure_provider', params: { provider: 'cursor', connect_local: true } },
        ], 'the new connection gets only its own automatic check')
        assert.equal(nodeSettings.parseProvider(render().find(({ props }) => props.item !== undefined)?.props.item).probe.authentication, 'authenticated')
        return
      }
      if (outcome === 'read-back' || unreadable) first.reject(new Error('client response timed out; outcome may be unknown'))
      else first.resolve(signedOut)
      await settleComponent(); render()
      if (unreadable) {
        assert.equal(calls.at(-1)?.method, 'node_settings', 'an unknown connection write is read back before any retry')
        readback.reject(new Error('device read failed'))
        await settleComponent(); render()
        focused(); focused()
        for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
        assert.equal(writes().length, 1, 'focus cannot resend an unknown write before it is reconciled')
        const elements = render()
        assert.equal(elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')?.props.disabled, true)
        if (outcome === 'same-device-reconnected') {
          const oldCalls = JSON.parse(JSON.stringify(calls))
          current = false; ready = replacement
          assert.equal(ready.entry.device_id, connection.entry.device_id, 'the component stays mounted for the same device')
          render(); await settleComponent(); render()
          assert.deepEqual(JSON.parse(JSON.stringify(replacementCalls)), [{ method: 'node_settings' }], 'the new connection waits for its own settings read')
          replacementRead.resolve(signedOut)
          for (let turn = 0; turn < 4; turn++) { await settleComponent(); render() }
          assert.deepEqual(JSON.parse(JSON.stringify(replacementCalls)), [
            { method: 'node_settings' }, { method: 'configure_provider', params: { provider: 'cursor', connect_local: true } },
          ], 'a successful read on the new connection automatically connects Cursor exactly once')
          assert.deepEqual(JSON.parse(JSON.stringify(calls)), oldCalls, 'reconnection never replays the unknown write on the old connection')
          const statuses = render().filter(({ props }) => props.item !== undefined).map(({ props }) => nodeSettings.parseProvider(props.item))
          assert.equal(statuses.find((item) => item.provider === 'cursor')?.probe.authentication, 'authenticated')
          assert.deepEqual(statuses.map((item) => ({ provider: item.provider, roles: item.roles })), [
            { provider: 'cursor', roles: [] }, { provider: 'codex', roles: ['execution'] },
          ], 'reconnection preserves provider roles')
          return
        }
        const retry: unknown = elements.find(({ type }) => type === 'InlineError')?.props.action
        assert.ok(retry && typeof retry === 'object' && 'label' in retry && retry.label === '重新读取' && 'onClick' in retry && typeof retry.onClick === 'function')
        retry.onClick()
      }
      for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
      assert.equal(writes().length, 2, 'the merged busy focus is consumed once after the connection is safe')
      assert.deepEqual(calls.map((call) => call.method), outcome === 'returned' || outcome === 'recheck-failed'
        ? ['node_settings', 'configure_provider', 'configure_provider']
        : outcome === 'read-back' ? ['node_settings', 'configure_provider', 'node_settings', 'configure_provider']
          : ['node_settings', 'configure_provider', 'node_settings', 'node_settings', 'configure_provider'], 'unknown writes are read before the queued check starts')
      if (outcome === 'recheck-failed') second.reject(new Error('client response timed out; outcome may be unknown'))
      else second.resolve(signedIn)
      for (let turn = 0; turn < 3; turn++) { await settleComponent(); render() }
      const card = render().find(({ props }) => props.item !== undefined)
      const status = nodeSettings.parseProvider(card?.props.item)
      assert.equal(status.probe.authentication, outcome === 'recheck-failed' ? 'sign_in_required' : 'authenticated')
      assert.deepEqual(status.roles, [])
      assert.equal(writes().length, 2, 'no further focus means no additional automatic attempt')
      if (outcome === 'recheck-failed') {
        assert.deepEqual(calls.map((call) => call.method), ['node_settings', 'configure_provider', 'configure_provider', 'node_settings'])
        assert.ok(render().some(({ type }) => type === 'InlineError'), 'a failed queued check remains visible instead of retrying itself')
      }
      assert.ok(writes().every((call) => JSON.stringify(call.params) === JSON.stringify({ provider: 'cursor', connect_local: true })), 'automatic connection never changes roles or other providers')
    } finally { app.close() }
  })
})


test("automatic connection checks the three local services without changing uses and reads back unknown writes", async (t) => {
  for (const outcome of ['ready', 'first-failed', 'unreadable', 'connection-changed']) await t.test(outcome, async () => {
    const provider: ProviderStatus = {
      provider: 'codex', authentication_source: 'private_login', roles: ['execution'], supports: ['agent', 'execution'], bin: '/fixture/codex', app_server: null,
      endpoint: null, execution_model: '', execution_readiness: { state: 'starting' }, setup: null, installed: true, home: null, sessions: null, env: [],
      state: 'starting', issues: [], readiness: { state: 'starting' },
      probe: { provider: 'codex', config_id: 'fixture', checked_at_ms: null, launch: { state: 'starting' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
      observed: { installed: { status: 'confirmed', evidence: 'binary_discovery' }, launchable: { status: 'unknown', evidence: 'app_server_starting' }, authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
    }
    const initial: NodeSettingsResponse = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: null }, providers: [provider], display_paths: {}, suggested_roots: [] }
    const first = pendingResult(), readback = pendingResult()
    let current = true, reads = 0
    const calls: { method: string; params: unknown }[] = []
    const connection = { isCurrent: () => current, request: async (method: string, params?: unknown) => {
      calls.push({ method, params })
      if (method === 'node_settings') return ++reads === 1 ? initial : readback.promise
      if (calls.filter((call) => call.method === 'configure_provider').length === 1) return first.promise
      if (outcome === 'unreadable') throw new Error('client response timed out; outcome may be unknown')
      return initial
    } }
    const workspace = { isCurrent: () => current, refresh: async () => {} }
    const timers = new Set<unknown>()
    let focused = () => {}
    const app = componentHarness('../devices/ai-settings.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: connection }) },
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => workspace },
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'Tasks' } } },
    }, {
      window: { addEventListener(_event: string, callback: () => void) { focused = callback }, removeEventListener() { focused = () => {} } },
      setTimeout(callback: () => void) { timers.add(callback); return callback }, clearTimeout(callback: unknown) { timers.delete(callback) },
    })
    const render = () => app.render('AiSettings', { compact: true, targetName: '远端工作机' })
    try {
      render(); await settleComponent()
      let elements = render()
      assert.ok(elements.some((element) => elementText(element).includes('在「远端工作机」上检测 Codex、Claude Code 和 Cursor')))
      const connect = elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')
      assert.ok(connect?.props.onClick)
      connect.props.onClick(); connect.props.onClick()
      assert.equal(calls.filter((call) => call.method === 'configure_provider').length, 1, 'duplicate clicks do not submit again')
      elements = render()
      assert.equal(timers.size, 0, 'starting-state polling pauses while the connection write is pending')
      focused()
      assert.equal(calls.length, 2, 'a focus read cannot replace the active write sequence')
      assert.equal(elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')?.props['aria-busy'], true)
      if (outcome === 'connection-changed') {
        current = false
        first.resolve({ ...initial, settings: { roots: ['/stale'], manager: null } })
        await settleComponent()
        assert.equal(calls.length, 2, 'a stale write cannot continue on a different connection')
        assert.ok(!render().some(({ props }) => Array.isArray(props.roots) && props.roots.includes('/stale')))
        return
      }
      if (outcome === 'ready') first.resolve(initial)
      else {
        first.reject(new Error('client response timed out; outcome may be unknown'))
        await settleComponent()
        assert.equal(calls.at(-1)?.method, 'node_settings')
        assert.equal(calls.filter((call) => call.method === 'configure_provider').length, 1, 'the unknown write is read back before continuing')
        if (outcome === 'unreadable') readback.reject(new Error('device read failed'))
        else readback.resolve(initial)
      }
      await settleComponent(); await settleComponent()
      const writes = calls.filter((call) => call.method === 'configure_provider').map((call) => JSON.parse(JSON.stringify(call.params)))
      assert.deepEqual(writes, [{ provider: 'codex', connect_local: true }, { provider: 'claude', connect_local: true }, { provider: 'cursor', connect_local: true }])
      elements = render()
      const button = elements.find(({ type, props }) => type === 'Button' && elementText(props.children) === '自动接入')
      assert.equal(button?.props.disabled, outcome === 'unreadable', 'a failed read blocks another attempt until the settings can be read')
      const result = elements.find(({ props }) => props.role === 'status')
      assert.match(elementText(result), outcome === 'ready' ? /检测已完成/ : /部分服务未完成/)
      if (outcome !== 'ready') assert.ok(elements.some(({ type }) => type === 'InlineError'), 'the first failure remains visible after the other service completes')
    } finally { app.close() }
  })
})


test("compact system provider cards use external service evidence without local prerequisites", async (t) => {
  for (const mode of ['system', 'spawn'] satisfies NonNullable<ProviderStatus['app_server']>['mode'][]) for (const installed of [false, true]) for (const authentication of ['authenticated', 'unverified', 'sign_in_required'] satisfies ProviderStatus['probe']['authentication'][]) await t.test(`${mode} installed=${installed} ${authentication}`, () => {
    const item: ProviderStatus = {
      provider: 'codex', authentication_source: 'private_login', roles: ['execution'], supports: ['agent', 'execution'], bin: '/fixture/codex',
      app_server: { mode, socket: '/fixture/codex.sock', link: { state: 'connected', socket: '/fixture/codex.sock', user_agent: 'fixture' } },
      endpoint: null, execution_model: '', execution_readiness: { state: 'ready' }, setup: { directory: null, command: 'codex login' }, installed, home: null, sessions: null,
      env: [{ name: 'FIXTURE_KEY', set: false, inherited: false, required: true }], state: 'ready', issues: [], readiness: { state: 'ready' },
      probe: { provider: 'codex', config_id: 'fixture', checked_at_ms: null, launch: { state: 'launchable' }, authentication, catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
      observed: { installed: { status: 'rejected', evidence: 'binary_discovery' }, launchable: { status: 'confirmed', evidence: 'app_server_handshake' }, authenticated: { status: 'confirmed', evidence: 'provider_probe' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
    }
    const response: NodeSettingsResponse = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: null }, providers: [item], display_paths: {}, suggested_roots: [] }
    const app = componentHarness('../devices/ai-settings.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'Tasks' }, secret: { icon: 'Key' } } },
    }, {}, 'ProviderCard')
    try {
      const elements = app.render('ProviderCard', { item, response, onboarding: false, compact: true, busy: '', models: null, error: '' })
      const summary = elements.find(({ type, props }) => type === 'span' && props.className === 'truncate')
      assert.equal(elementText(summary), mode === 'system' || installed ? '可用' : '未安装', 'the main state follows readiness for system services')
      const diagnostics = elements.find(({ type, props }) => type === 'Details' && props.label === '程序、认证与模型详情')
      assert.ok(diagnostics, 'diagnostic evidence remains available behind a named disclosure')
      assert.equal(diagnostics.props.open, undefined, 'secondary diagnostics start collapsed')
      const evidence = elementText(diagnostics)
      const login = elements.find(({ type, props }) => type === 'Details' && props.label === '登录方式')
      assert.equal(Boolean(login), authentication === 'sign_in_required', 'login instructions only appear when login is required')
      if (login) {
        assert.equal(login.props.open, true, 'a required login next step remains expanded')
        assert.match(elementText(login), /在运行 AI 的电脑上执行以下命令/)
        const command = elements.find(({ type, props }) => type === 'Input' && props.label === 'Codex 登录命令')
        assert.equal(command?.props.value, 'codex login')
        assert.equal(command?.props.readOnly, true)
      }
      assert.ok(elements.some(({ type, props }) => type === 'Button' && props['aria-label'] === 'Codex 检查可用性' && !props.disabled), 'the check action stays available outside diagnostic disclosure')
      if (mode === 'system') {
        assert.match(evidence, /程序外部服务/)
        assert.ok(evidence.includes(`认证${{ authenticated: '已认证', unverified: '未验证', sign_in_required: '需登录' }[authentication]}`))
        assert.equal(elements.filter(({ props }) => props.name === 'FIXTURE_KEY').length, 0, 'an external service never requests the private key')
        assert.ok(!elements.some((element) => elementText(element).includes('找不到 Codex 程序')))
      } else {
        assert.ok(evidence.includes(`程序${installed ? '已找到' : '未找到'}`))
        assert.match(evidence, /认证需填写密钥/)
        assert.equal(elements.filter(({ props }) => props.name === 'FIXTURE_KEY').length, installed ? 1 : 0)
        assert.equal(elements.some((element) => elementText(element).includes('找不到 Codex 程序')), !installed)
      }
    } finally { app.close() }
  })
})


test("provider cards keep summary readiness, setup and execution details in their own roles", async (t) => {
  const ready: ProviderReadiness = { state: 'ready' }
  const signIn: ProviderReadiness = { state: 'needs_setup', issue: { stage: 'authentication', code: 'sign_in_required' } }
  const model: ProviderReadiness = { state: 'unverified', issue: { stage: 'model_list', code: 'model_unverified' } }
  const signInText = 'Codex 需要完成认证，请在运行设备登录或配置 API 密钥'
  const modelText = 'Codex 尚未确认这个模型，可发送消息或先检查可用性'
  const cases: (Pick<ProviderStatus, 'roles' | 'readiness' | 'execution_readiness'> & {
    name: string; label: string; reason: string; setup: boolean; executionLabel: string | null; executionReason: string
  })[] = [
    { name: 'execution-only unverified', roles: ['execution'], readiness: ready, execution_readiness: model, label: '未验证，可尝试', reason: modelText, setup: true, executionLabel: '未验证，可尝试', executionReason: modelText },
    { name: 'execution-only ready', roles: ['execution'], readiness: signIn, execution_readiness: ready, label: '可用', reason: '', setup: false, executionLabel: '可用', executionReason: '' },
    { name: 'Bot ready and execution unverified', roles: ['agent', 'execution'], readiness: ready, execution_readiness: model, label: '可用', reason: '', setup: false, executionLabel: '未验证，可尝试', executionReason: modelText },
    { name: 'Bot needs setup and execution ready', roles: ['execution', 'agent'], readiness: signIn, execution_readiness: ready, label: '需设置', reason: signInText, setup: true, executionLabel: '可用', executionReason: '' },
    { name: 'no roles', roles: [], readiness: signIn, execution_readiness: ready, label: '需设置', reason: signInText, setup: false, executionLabel: null, executionReason: '' },
  ]
  const text = (node: unknown): string => typeof node === 'string' ? node : Array.isArray(node) ? node.map(text).join('')
    : node && typeof node === 'object' && 'props' in node && node.props && typeof node.props === 'object' && 'children' in node.props ? text(node.props.children) : ''
  const kit = { toneText: { neutral: 'neutral', success: 'success', warning: 'warning' }, providerIcons: { codex: 'CodexIcon' }, Details: 'Details', FolderPath: 'FolderPath', CopyableId: 'CopyableId', BusyIcon: 'BusyIcon' }
  for (const platform of ['web', 'mobile']) for (const example of cases) await t.test(`${platform} ${example.name}`, () => {
    const item: ProviderStatus = {
      provider: 'codex', authentication_source: 'private_login', roles: example.roles, supports: ['agent', 'execution'], bin: '/usr/bin/codex', app_server: null,
      endpoint: null, execution_model: 'saved-model', execution_readiness: example.execution_readiness,
      setup: { directory: '/private/provider', command: 'codex --home /private/provider login' }, installed: true,
      home: null, sessions: null, env: [], state: example.readiness.state, issues: [], readiness: example.readiness,
      observed: { installed: { status: 'confirmed', evidence: 'binary_discovery' }, launchable: { status: 'unknown', evidence: 'launch_prerequisites' }, authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' } },
      probe: { provider: 'codex', config_id: 'config', checked_at_ms: null, launch: { state: 'launchable' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null },
    }
    const response: NodeSettingsResponse = {
      settings: { roots: [], manager: item.roles.includes('agent') ? { provider: 'codex', workspace: '/project', model: 'bot-model', reasoning_effort: null, backup: { automatic: false, candidates: [] } } : null },
      defaults: { workspace: '/project' }, readiness: { manager: item.readiness }, providers: [item], display_paths: {}, suggested_roots: [],
    }
    const name = platform === 'web' ? 'ProviderCard' : 'ProviderRow'
    const app = componentHarness(platform === 'web' ? '../devices/ai-settings.tsx' : '../../../../../apps/mobile/app/ai.tsx', {
      '@vibeface/client/lib/node.ts': nodeSettings,
      '@vibeface/client/identity/copy.ts': { deviceCopy },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
      '@/lib/glossary': { terms: { bot: { icon: 'Bot' }, task: { icon: 'ListChecks' } } },
      '@/components/arc/product-kit': kit,
      '@/components/kit': kit,
    }, {}, name)
    const props = { item, response, onboarding: false, first: true, busy: '', models: null, error: '' }
    try {
      let elements = app.render(name, props)
      if (platform === 'mobile') {
        assert.equal(elements.some(({ props }) => props.label === '执行任务'), false, 'execution details start collapsed')
        const expand = elements.find(({ type, props }) => (type === 'Pressable' || type === 'Button') && props.accessibilityRole === 'button')?.props.onPress
        assert.ok(typeof expand === 'function')
        expand()
        elements = app.render(name, props)
      }
      const children = elements.at(-1)?.props.children
      assert.ok(Array.isArray(children))
      assert.equal(text(children[0]), `Codex${example.label}`, 'the row header shows the selected summary state')
      const summary = text(children[2])
      for (const reason of [signInText, modelText]) assert.equal(summary.includes(reason), example.reason === reason, 'the summary shows only its own reason')
      assert.equal(elements.filter(({ props }) => props.value === item.setup?.command).length, example.setup ? 1 : 0, 'setup follows the selected readiness and enabled roles')
      const execution = elements.find(({ props }) => props.label === '执行任务')
      if (example.executionLabel === null) assert.equal(execution, undefined)
      else {
        assert.ok(execution)
        assert.equal(text(execution), `执行模型：${example.executionLabel}${example.executionReason}`, 'execution details show the saved execution readiness independently')
      }
    } finally { app.close() }
  })
})


test('webhook secret saves preserve failures and newer drafts on web and mobile', async (t) => {
  const settings = workspaceModel.parseTriageSettings({
    default_outlet: 'record', projects: [], webhook_url: '', webhook_secret_set: false,
    scan_codex_sessions: false, codex_sessions: null, decision_url: '', device_id: 'node', decision_tokens: [],
    outbox: { pending: 0, delivered: 0, dead: 0 },
    status: { budget: { day: '2026-11-11', used: 0, limit: 10, retry_at_ms: 0, paused_reason: null }, pending_events: 0, inflight_events: 0, unknown_events: 0 },
  })
  for (const platform of ['web', 'mobile']) for (const outcome of ['failed', 'saved', 'edited']) await t.test(`${platform} ${outcome}`, async () => {
    const held = pendingResult(), writes: unknown[] = []
    const save = (params: unknown) => { writes.push(params); return held.promise }
    const connection = { isCurrent: () => true, request: async (method: string, params?: unknown) => method === 'triage_settings' ? settings : save(params) }
    const name = platform === 'mobile' ? 'Triage' : 'TriageSettings'
    const app = componentHarness(platform === 'mobile' ? '../../../../../apps/mobile/app/external-chat.tsx' : '../devices/triage-settings.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: connection }) },
      '@vibeface/client/identity/copy.ts': { waitingCopy },
      '@vibeface/client/workspace/model.ts': workspaceModel,
      '@/lib/glossary': { terms: { externalChat: { label: '外部聊天', icon: 'Send' } } },
      '@/features/devices/settings-resource': { useSettingsResource: () => ({ value: settings, busy: '', error: null, call: (_key: string, _method: string, params: unknown) => save(params) }) },
      'expo-clipboard': { setStringAsync: async () => assert.fail('Saving webhook settings must not copy clipboard text') },
      'react-native': { View: 'View', AccessibilityInfo: { announceForAccessibility: () => assert.fail('Saving webhook settings must not announce clipboard feedback') } },
    }, {}, platform === 'mobile' ? 'Triage' : '')
    const render = () => app.render(name)
    const field = () => {
      const input = render().find(({ props }) => platform === 'mobile' ? props.accessibilityLabel === '外部聊天密钥' : props.id === 'webhook-secret')
      assert.ok(input)
      return input.props
    }
    const edit = (value: string) => {
      const input = field()
      if (platform === 'mobile') { assert.ok(typeof input.onChangeText === 'function'); input.onChangeText(value) }
      else { assert.ok(typeof input.onChange === 'function'); input.onChange({ target: { value } }) }
    }
    try {
      render(); await settleComponent()
      if (platform === 'web') {
        const expand = render().find(({ type }) => type === 'SettingsRow')?.props.onClick
        assert.ok(typeof expand === 'function'); expand()
      }
      edit('first-secret-draft')
      if (platform === 'mobile') {
        const press = render().find(({ type, props }) => type === 'Button' && elementText(props.children) === '保存')?.props.onPress
        assert.ok(typeof press === 'function'); press()
      } else {
        const submit = render().find(({ type }) => type === 'form')?.props.onSubmit
        assert.ok(typeof submit === 'function'); submit({ preventDefault() {} })
      }
      assert.equal(writes.length, 1)
      assert.equal(JSON.parse(JSON.stringify(writes[0])).webhook_secret, 'first-secret-draft')
      if (outcome === 'edited') edit('newer-secret-draft')
      if (outcome === 'failed' && platform === 'web') held.reject(new Error('device offline'))
      else held.resolve(outcome === 'failed' ? null : settings)
      await settleComponent()
      assert.equal(field().value, outcome === 'saved' ? '' : outcome === 'edited' ? 'newer-secret-draft' : 'first-secret-draft')
    } finally { app.close() }
  })
})
