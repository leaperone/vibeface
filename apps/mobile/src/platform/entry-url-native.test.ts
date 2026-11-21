

import { decodeOutbox } from '@vibeface/client/workspace/outbox-codec.ts'




import type { GenerationAction } from '@vibeface/client/lib/use-generation-action.ts'



import assert from "node:assert/strict"

import test from "node:test"




import * as recoveryLinks from '@vibeface/client/identity/recovery-link.ts'
import { entryAddress, messageOf, providerNames } from "@vibeface/client/lib/node.ts"
import * as nodeSettings from "@vibeface/client/lib/node.ts"

import type { NodeSettingsResponse } from "@vibeface/client/lib/node.ts"
import type { ManagerJob, WorkspaceChannel, WorkspaceCommand, WorkspaceFilter } from "@vibeface/client/workspace/types.ts"

import { parseConversation, parseNodes } from "@vibeface/client/workspace/model.ts"
import { placeChanges, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { answerLabel, attentionLabel, approvalTask, declined, foldAnswers, ignoredAnswer, questionMeta, questionOpen, settledAnswer, sharedProject, questionParts } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { joinDestination } from '@vibeface/client/identity/switch-network.ts'
import { joinTicket } from '@vibeface/client/lib/transport.ts'

import { promptIntent, botReach, canRetryJob, runtimeLabel, composerHint } from "@vibeface/client/chat/protocol.ts"
import { joinCopy, offlineCopy } from "@vibeface/client/identity/copy.ts"
import { chatTimeline } from "@vibeface/client/chat/timeline.ts"
import { accessOf } from "@vibeface/client/identity/access.ts"

import { unreachable } from "@vibeface/client/workspace/home.ts"

import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"

import type { NotificationDestination } from '@vibeface/client/lib/platform.ts'

import * as conversationCache from '@vibeface/client/workspace/conversation-cache.ts'


import { context, componentHarness, pendingResult, nestedElements, componentText, findActionProps, parityBot, parityNode, parityWork, parityShared, nativeChatHarness, nativeFileFixture, settleComponent, type HarnessElement } from '../../../app/src/features/workspace/entry-url-test-kit.test-support.ts'
import { isLegacyRecoveryFile } from './legacy-recovery.ts'
import * as lastSeenModel from '../data/last-seen.ts'
import { notificationOpenings } from './notification-opens.ts'
import { mobileOutbox } from './outbox.ts'

test('native Offline uses validated display scopes and fences late pending rows and errors while keeping corrupt-ledger cache readable', async () => {
  const alpha = { network_id: 'a'.repeat(64), device_id: 'b'.repeat(64), entry_device_id: 'c'.repeat(64) }
  const beta = { network_id: 'd'.repeat(64), device_id: 'e'.repeat(64), entry_device_id: 'f'.repeat(64) }
  let scope: typeof alpha | null = alpha
  const storage = new Map<string, string>()
  const keyValue = { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => { storage.set(key, value) } }
  const cache = conversationCache.createConversationCache({ storage: keyValue, now: () => 10 })
  const seen = (owner: typeof alpha) => ({ ...owner, entry_label: '上次的工作设备', bots: [parityBot], primary: 'chat', members: [
    { device_id: owner.device_id, name: '手机', kind: 'client', manage: true, bot: null },
    { device_id: owner.entry_device_id, name: '工作设备', kind: 'node', manage: false, bot: 'ready' },
  ] })
  const select = (owner: typeof alpha) => { assert.ok(lastSeenModel.decodeLastSeen(seen(owner), owner)); scope = owner; storage.set('vibeface.last-seen', JSON.stringify(seen(owner))) }
  for (const [owner, text] of [[alpha, 'Alpha 已同步正文'], [beta, 'Beta 已同步正文']] satisfies [typeof alpha, string][]) {
    cache.save({ networkId: owner.network_id, deviceId: owner.device_id, conversationId: 'chat', payload: parseConversation({ ...context, messages: [{ message_id: text, event_hash: text, role: 'assistant', text, created_at_ms: 1 }] }) })
  }
  const pending = (owner: typeof alpha, text: string) => { const item = { key: `${owner.network_id}.${owner.device_id}:message:${text}`, scope_id: 'chat', method: 'message', created_at_ms: 2, delivery: 'saved', entry_device_id: owner.entry_device_id,
    params: { conversation_id: 'chat', message_id: text, target: { kind: 'bot_main', bot_id: parityBot.bot_id }, intent: 'continue', sequence: 1, text, report_results: true, attestation: { device_id: owner.device_id, signature: '0'.repeat(128) } } }; return decodeOutbox(item, item.key) }
  const reads: ReturnType<typeof pendingResult>[] = []
  const bind = () => {}
  let joinArguments: unknown[] = []
  const device = { get displayScope() { return scope }, ready: null, phase: 'reconnecting', entries: [{ kind: 'gateway', device_id: alpha.entry_device_id, address: 'https://node.example' }],
    link: { kind: 'offline' }, entry: { kind: 'gateway', device_id: alpha.entry_device_id, address: 'https://node.example' }, entryName: () => '工作设备', connectionGeneration: () => 1, join(...args: unknown[]) { joinArguments = args }, cancelJoin() {}, leave() {} }
  select(alpha)
  const app = componentHarness('../../../../../apps/mobile/src/data/live.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/identity/access.ts': { accessOf },
    '@vibeface/client/lib/node.ts': { entryAddress, messageOf }, '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/chat/timeline.ts': { chatTimeline }, '@vibeface/client/workspace/questions.ts': { foldAnswers },
    '@/platform': { mobilePlatform: { storage: keyValue, outbox: { all: () => { const read = pendingResult(); reads.push(read); return read.promise } } } },
    './last-seen': lastSeenModel, './context': { WorkspaceProvider: 'NativeWorkspace' },
  }, {}, 'Offline')
  const render = () => {
    const value = app.render('Offline').find(({ type }) => type === 'NativeWorkspace')?.props.value
    return workspaceModel.record(value)
  }
  const texts = (value: ReturnType<typeof render>) => {
    const messages = workspaceModel.record(workspaceModel.record(value.conversations).chat).messages
    assert.ok(Array.isArray(messages)); return Array.from(messages, message => workspaceModel.record(message).text)
  }
  try {
    assert.deepEqual(texts(render()), ['Alpha 已同步正文']); assert.equal(reads.length, 1)
    const join = workspaceModel.record(render().actions).join
    assert.ok(typeof join === 'function')
    await join('vibeface://join/fixture', '手机', bind)
    assert.deepEqual(joinArguments, ['vibeface://join/fixture', '手机', bind])
    select(beta)
    assert.deepEqual(texts(render()), ['Beta 已同步正文']); assert.equal(reads.length, 2)
    reads[0].resolve([pending(alpha, 'Alpha 迟到待发')]); await settleComponent()
    assert.deepEqual(texts(render()), ['Beta 已同步正文'], 'the previous scope cannot append a late pending input')
    reads[1].resolve([pending(beta, 'Beta 待发')]); await settleComponent()
    assert.deepEqual(texts(render()), ['Beta 已同步正文', 'Beta 待发'])
    select(alpha)
    assert.deepEqual(texts(render()), ['Alpha 已同步正文'], 'the first render after changing scope cannot flash the previous pending state')
    assert.equal(reads.length, 3)
    select(beta); render(); assert.equal(reads.length, 4)
    reads[2].reject(new Error('Alpha 迟到账本错误')); await settleComponent()
    assert.doesNotMatch(String(workspaceModel.record(workspaceModel.record(render().conversations).chat).blocked), /Alpha 迟到/)
    reads[3].reject(new Error('本地待发账本损坏')); await settleComponent()
    const blocked = render()
    assert.deepEqual(texts(blocked), ['Beta 已同步正文'])
    assert.equal(workspaceModel.record(workspaceModel.record(blocked.conversations).chat).blocked, '本地待发账本损坏', 'a damaged ledger is visible while the cached transcript remains readable')
    select(alpha)
    assert.doesNotMatch(String(workspaceModel.record(workspaceModel.record(render().conversations).chat).blocked), /损坏/, 'a previous scope error cannot flash before the next outbox effect completes')
    reads[4].resolve([]); await settleComponent()
    storage.set('vibeface.last-seen', JSON.stringify(seen(beta)))
    scope = { ...alpha, entry_device_id: '1'.repeat(64) }
    const mismatched = render()
    assert.deepEqual(Object.keys(workspaceModel.record(mismatched.conversations)), [])
    assert.ok(Array.isArray(mismatched.bots)); assert.deepEqual(Array.from(mismatched.bots), [], 'a stored snapshot cannot choose a different display identity or entry')
    assert.equal(mismatched.networkId, alpha.network_id); assert.equal(reads.length, 5)
    scope = null
    const unsigned = render()
    assert.equal(unsigned.networkId, null); assert.equal(unsigned.deviceId, null)
    assert.deepEqual(Object.keys(workspaceModel.record(unsigned.conversations)), [])
  } finally { app.close() }
})


test('Mobile channel drafts load once, retain their revision and reset with their owner', async () => {
  const path = '../../../../../apps/mobile/src/features/workspace/workspace.tsx'
  const channel = (id: string, revision = 4): WorkspaceChannel => ({ channel_id: id, title: `频道 ${id}`, purpose: `目标 ${id}`, members: ['b'], coordinator_bot: 'b', coordinator_node: 'worker', owner_epoch: 2, revision, event_hash: `event-${revision}`, created_at_ms: 1 })
  const ready = { principal: { network_id: 'network-one', device_id: 'phone' }, entry: { device_id: 'entry' } }
  let generation = 1, current = true, save = pendingResult(), nextId = 0
  const coldRead = pendingResult(), reads: WorkspaceFilter[] = []
  const commands: WorkspaceCommand[] = [], navigations: string[] = [], channels: WorkspaceChannel[] = Array.from({ length: 256 }, (_,index) => channel(`window-${index}`))
  const shared = {
    generation, writable: true, workspaceState: { channels }, isCurrent: () => current,
    readWorkspaceState: (filter: WorkspaceFilter) => { reads.push(filter); return reads.length === 1 ? coldRead.promise : Promise.resolve({ channels: shared.workspaceState.channels.filter((item) => item.channel_id === filter.channel_id) }) },
    bots: [{ bot_id: 'a', name: 'Bot A' }, { bot_id: 'b', name: 'Bot B' }],
    nodes: [{ device_id: 'entry', name: '入口', online: true, revoked: false }, { device_id: 'worker', name: '工作机', online: true, revoked: false }],
    workspaceCommand: (command: WorkspaceCommand) => { commands.push(command); return save.promise },
  }
  const dependencies = {
    '@/data/live': { useLive: () => ({ ...shared }) },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready, connectionGeneration: () => generation }) },
    '@vibeface/client/lib/node.ts': { messageOf },
    '@vibeface/client/workspace/storage.ts': { newId: () => `new-${++nextId}` },
    '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    'expo-router': { Stack: { Screen: 'Screen' }, useRouter: () => ({ replace: (url: string) => navigations.push(url) }) },
  }
  const screen = componentHarness(path, dependencies)
  let form: ReturnType<typeof componentHarness> | undefined, key: string | undefined
  const render = (channelId?: string) => {
    const child = screen.render('ChannelSettingsScreen', { channelId }).find(({ type }) => typeof type === 'function' && type.name === 'ChannelForm')
    if (!child) { form?.close(); form = undefined; key = undefined; return [] }
    if (child.key !== key) {
      form?.close()
      form = componentHarness(path, dependencies, {}, 'ChannelForm', { '@/lib/use-run': '../../../../../apps/mobile/src/lib/use-run.ts' })
      key = child.key
    }
    assert.ok(form)
    return form.render('ChannelForm', child.props)
  }
  const field = (label: string, id?: string) => render(id).find(({ type, props }) => type === 'TextInput' && props.accessibilityLabel === label)?.props
  const change = (label: string, value: string, id?: string) => { const action = field(label, id)?.onChangeText; assert.equal(typeof action, 'function'); if (typeof action === 'function') action(value) }
  const press = (label: string, id?: string) => { const action = render(id).find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props.onPress; assert.equal(typeof action, 'function'); if (typeof action === 'function') action() }
  const selected = (id: string) => render(id).filter(({ props }) => props.accessibilityState && workspaceModel.record(props.accessibilityState).selected === true).map(({ props }) => componentText(props.children))
  try {
    assert.equal(render('one').length, 0)
    assert.equal(screen.render('ChannelSettingsScreen', { channelId: 'one' }).find(({ type }) => type === 'EmptyState')?.props.title, '正在读取频道设置')
    assert.deepEqual(JSON.parse(JSON.stringify(reads)), [{ channel_id: 'one' }])
    coldRead.resolve({ channels: [channel('one')] }); await settleComponent()
    assert.equal(field('频道名称', 'one')?.value, '频道 one')
    assert.equal(field('频道目标', 'one')?.value, '目标 one')
    assert.deepEqual(selected('one'), ['Bot B', '工作机'])
    assert.deepEqual(render('one').filter(({ type, props }) => type === 'Pressable' && workspaceModel.record(props.accessibilityState).checked === true).map(({ props }) => props.accessibilityLabel), ['Bot B'])
    change('频道名称', '本地标题', 'one'); change('频道目标', '本地目标', 'one')
    const member = render('one').find(({ type, props }) => type === 'Pressable' && props.accessibilityLabel === 'Bot A')?.props.onPress
    assert.equal(typeof member, 'function'); if (typeof member === 'function') member()
    press('Bot A', 'one'); press('入口', 'one')
    shared.workspaceState.channels = [{ ...channel('one', 5), title: '远端标题', purpose: '远端目标', owner_epoch: 3 }]
    assert.equal(field('频道名称', 'one')?.value, '本地标题')
    assert.equal(field('频道目标', 'one')?.value, '本地目标')
    assert.deepEqual(selected('one'), ['Bot A', '入口'])
    assert.match(componentText(form?.output), /频道设置已在其他地方更新/)
    assert.equal(reads.length, 1, 'ordinary shared object and revision changes never restart the scoped read')
    press('保存频道', 'one'); press('正在保存', 'one')
    assert.equal(commands.length, 1, 'pending saves do not submit twice')
    assert.deepEqual(JSON.parse(JSON.stringify(commands[0])), { kind: 'configure_channel', channel_id: 'one', title: '本地标题', purpose: '本地目标', members: ['b', 'a'], coordinator_bot: 'a', coordinator_node: 'entry', expected_revision: 4, owner_epoch: 2 })
    save.reject(new Error('保存结果尚未确认')); await settleComponent()
    assert.equal(field('频道名称', 'one')?.editable, false)
    assert.equal(render('one').find(({ type }) => type === 'ErrorText')?.props.error, '保存结果尚未确认')
    save = pendingResult(); press('保存频道', 'one')
    assert.equal(commands[1], commands[0], 'unknown-result retry preserves the original command identity')
    save.resolve(undefined); await settleComponent()
    assert.deepEqual(navigations, ['/channel/one'])

    shared.workspaceState.channels.push(channel('two', 9))
    render('two'); await settleComponent()
    assert.equal(field('频道名称', 'two')?.value, '频道 two')
    save = pendingResult(); press('保存频道', 'two')
    const lateId = save
    screen.render('ChannelSettingsScreen', { channelId: 'one' })
    lateId.resolve(undefined); await settleComponent()
    assert.deepEqual(navigations, ['/channel/one'], 'a previous route never navigates even before its form unmounts')
    assert.equal(field('频道名称', 'one')?.value, '远端标题')
    assert.equal(field('频道名称', 'one')?.editable, true)
    assert.equal(render('one').find(({ type }) => type === 'ErrorText'), undefined)

    change('频道名称', '旧网络草稿', 'one')
    save = pendingResult(); press('保存频道', 'one')
    const lateNetwork = save
    ready.principal.network_id = 'network-two'
    shared.workspaceState.channels = [{ ...channel('one', 12), title: '新网络频道' }]
    render('one'); await settleComponent()
    assert.equal(field('频道名称', 'one')?.value, '新网络频道')
    lateNetwork.resolve(undefined); await settleComponent()
    assert.deepEqual(navigations, ['/channel/one'])
    change('频道名称', '旧设备草稿', 'one'); ready.principal.device_id = 'another-phone'
    render('one'); await settleComponent()
    assert.equal(field('频道名称', 'one')?.value, '新网络频道')
    change('频道名称', '旧连接草稿', 'one'); generation++; shared.generation = generation
    render('one'); await settleComponent()
    assert.equal(field('频道名称', 'one')?.value, '新网络频道')
    save = pendingResult(); press('保存频道', 'one'); current = false
    save.resolve(undefined); await settleComponent()
    assert.deepEqual(navigations, ['/channel/one'], 'a stale workspace never navigates after save')
    current = true

    assert.equal(field('频道名称')?.value, '')
    change('频道名称', '新频道')
    save = pendingResult(); press('创建频道')
    assert.deepEqual(JSON.parse(JSON.stringify(commands.at(-1))), { kind: 'create_channel', channel_id: 'new-1', title: '新频道', purpose: '', members: ['a', 'b'], coordinator_bot: 'a', coordinator_node: 'entry' })
    form?.close(); form = undefined
    save.resolve(undefined); await settleComponent()
    assert.deepEqual(navigations, ['/channel/one'], 'an unmounted form never navigates after save')
  } finally { form?.close(); screen.close() }
})


test('Mobile channel scoped reads settle missing records, retry failures and ignore stale owners', async () => {
  const ready = { principal: { network_id: 'network', device_id: 'phone' }, entry: { device_id: 'entry' } }
  let current = true
  const requests: { filter: WorkspaceFilter; result: ReturnType<typeof pendingResult> }[] = []
  const shared = {
    generation: 1, workspaceState: { channels: [] }, isCurrent: () => current,
    readWorkspaceState: (filter: WorkspaceFilter) => { const result = pendingResult(); requests.push({ filter, result }); return result.promise },
  }
  const app = componentHarness('../../../../../apps/mobile/src/features/workspace/workspace.tsx', {
    '@/data/live': { useLive: () => ({ ...shared }) },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready }) },
    '@vibeface/client/lib/node.ts': { messageOf }, 'expo-router': { Stack: { Screen: 'Screen' } },
  })
  const render = (channelId?: string) => app.render('ChannelSettingsScreen', { channelId })
  const notice = (id: string) => render(id).find(({ type }) => type === 'EmptyState')?.props.title
  const form = (id: string) => render(id).find(({ type }) => typeof type === 'function' && type.name === 'ChannelForm')
  const retry = (id: string) => {
    const action = render(id).find(({ type, props }) => type === 'Button' && componentText(props.children) === '重新读取')?.props.onPress
    assert.equal(typeof action, 'function'); if (typeof action === 'function') action()
    render(id)
  }
  const response = (id: string) => ({ channels: [{ channel_id: id, title: `频道 ${id}`, purpose: '明确的目标', members: ['bot'], coordinator_bot: 'bot', coordinator_node: 'worker', revision: 7, owner_epoch: 3 }] })
  try {
    assert.equal(notice('missing'), '正在读取频道设置')
    assert.deepEqual(JSON.parse(JSON.stringify(requests[0].filter)), { channel_id: 'missing' })
    requests[0].result.resolve({ channels: [] }); await settleComponent()
    assert.equal(notice('missing'), '没有找到这个频道')
    assert.equal(form('missing'), undefined, 'not-found never becomes a create form')
    retry('missing')
    assert.equal(notice('missing'), '正在读取频道设置')
    assert.equal(requests.length, 2)
    requests[1].result.reject(new Error('无法读取这个频道')); await settleComponent()
    assert.equal(notice('missing'), '无法读取这个频道')
    assert.equal(form('missing'), undefined)
    retry('missing')
    assert.deepEqual(JSON.parse(JSON.stringify(requests[2].filter)), { channel_id: 'missing' })
    requests[2].result.resolve(response('missing')); await settleComponent()
    assert.ok(form('missing'))
    assert.equal(notice('missing'), undefined)

    for (const outcome of ['resolve', 'reject']) {
      render(`old-${outcome}`)
      const old = requests.at(-1)
      assert.ok(old)
      ready.principal.network_id = `network-${outcome}`
      assert.equal(notice('new'), '正在读取频道设置')
      const fresh = requests.at(-1)
      assert.ok(fresh && fresh !== old)
      if (outcome === 'resolve') old.result.resolve(response(`old-${outcome}`))
      else old.result.reject(new Error('旧网络读取失败'))
      await settleComponent()
      assert.equal(notice('new'), '正在读取频道设置', 'an old response or error cannot settle the new owner')
      assert.equal(form('new'), undefined)
      fresh.result.resolve(response('new')); await settleComponent()
      assert.ok(form('new'))
      assert.equal(notice('new'), undefined)
    }
    render('disconnected'); current = false
    requests.at(-1)?.result.resolve(response('disconnected')); await settleComponent()
    assert.equal(form('disconnected'), undefined, 'a non-current workspace cannot accept a response')
    current = true
    render('unmounted')
    app.close()
    requests.at(-1)?.result.reject(new Error('卸载后读取失败')); await settleComponent()
    assert.equal(notice('unmounted'), '正在读取频道设置', 'an unmounted screen ignores late failures')
    const count = requests.length
    render()
    assert.equal(requests.length, count, 'create mode never starts a scoped channel read')
  } finally { app.close() }
})


test('the native Arc composer keeps the same editor and attachment nodes inside its panel', () => {
  const app = componentHarness('../../../../../apps/mobile/src/components/arc/chat.tsx', { 'react-native': { View: 'View', TextInput: 'TextInput' }, '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') } })
  const editor = { type: 'TextInput', props: { value: '保留输入中的草稿', selection: { start: 2, end: 2 } } }
  const attachments = { type: 'View', props: { children: '待发送文件' } }
  const actions = { type: 'Button', props: { children: '发送' } }
  try {
    const elements = app.render('ComposerPanel', { editor, attachments, actions })
    const root = elements.at(-1)
    assert.equal(root?.type, 'View')
    assert.ok(Array.isArray(root?.props.children))
    assert.equal(root.props.children[0], attachments)
    assert.equal(root.props.children[1], editor, 'layout leaves the editor and its selection intact')
    assert.equal(elements.find(({ props }) => props.children === actions)?.type, 'View')
  } finally { app.close() }
})


test("mobile provider probes discard settings that arrive after another call or connection", async () => {
  const settings = (root: string): NodeSettingsResponse => ({
    settings: { roots: [root], manager: null }, defaults: { workspace: '/project' },
    readiness: { manager: null }, providers: [], display_paths: {}, suggested_roots: [],
  })
  const initial = settings('/initial'), configured = settings('/new'), late = settings('/late')
  for (const changed of ['call', 'connection', 'none']) {
    const held = pendingResult()
    let current = true, reads = 0, snapshot: unknown
    const connection = { isCurrent: () => current, request: async (method: string) => {
      if (method === 'node_settings') return ++reads === 1 ? initial : held.promise
      return method === 'configure_node' ? configured : 'probe result'
    } }
    const app = componentHarness('../../../../../apps/mobile/src/features/devices/settings-resource.ts', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: connection }) },
      '@vibeface/client/lib/node.ts': nodeSettings,
    })
    const useSettingsResource = app.module.useSettingsResource, nodeSettingsResource = app.module.nodeSettingsResource
    assert.ok(typeof useSettingsResource === 'function')
    app.module.Read = () => { snapshot = useSettingsResource(nodeSettingsResource); return null }
    const state = () => {
      app.render('Read')
      assert.ok(snapshot && typeof snapshot === 'object' && 'call' in snapshot && typeof snapshot.call === 'function' && 'value' in snapshot)
      return { value: snapshot.value, call: snapshot.call }
    }
    try {
      state(); await settleComponent()
      assert.deepEqual(state().value, initial)
      const probe = state().call('probe', 'provider_probe', { provider: 'codex' })
      await settleComponent()
      assert.equal(reads, 2)
      if (changed === 'call') await state().call('save', 'configure_node', { roots: ['/new'], manager: null })
      if (changed === 'connection') current = false
      held.resolve(late)
      assert.equal(await probe, changed === 'none' ? 'probe result' : null)
      assert.deepEqual(state().value, changed === 'none' ? late : changed === 'call' ? configured : initial)
    } finally { app.close() }
  }
})


test('native context and task detail keep selection errors and foreign payloads bound to their route and connection', async () => {
  for (const route of ['context', 'work']) {
    let generation = 1
    let shared = parityShared()
    const old = pendingResult(), next = pendingResult()
    const views: string[] = []
    shared.conversationId = 'other-chat'
    shared.data = parseConversation({ ...context, conversation: { ...context.conversation, conversation_id: 'other-chat' }, works: [parityWork({ title: '别的 Bot 的秘密记录' })] })
    shared.synced = false
    shared.isCurrent = () => generation === 1
    shared.selectBot = () => old.promise.then(() => {})
    shared.setView = (view) => { views.push(view) }
    const app = componentHarness(route === 'context' ? '../../../../../apps/mobile/app/b/[botId]/context.tsx' : '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot', workId: 'work' }), useRouter: () => ({ push() {}, back() {} }) },
      '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
      '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel,
      '@vibeface/client/workspace/questions.ts': workspaceQuestions, '@vibeface/client/lib/errors.ts': errors, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@/components/kit': { EmptyState: 'EmptyState', Details: 'Details', providerIcons: { codex: 'CodexIcon' }, toneText: {} },
    }, {}, route === 'work' ? 'TaskDetail' : '')
    const render = () => app.render(route === 'work' ? 'TaskDetail' : 'default', { botId: 'bot', workId: 'work' })
    const emptyTitle = (elements: ReturnType<typeof app.render>) => elements.find(({ type }) => type === 'EmptyState')?.props.title
    try {
      assert.match(String(emptyTitle(render())), /正在读取/)
      assert.deepEqual(views, ['other'], 'read-only routes never acknowledge chat notifications')
      generation = 2
      shared = { ...shared, generation, isCurrent: () => generation === 2, selectBot: () => next.promise.then(() => {}) }
      render()
      next.reject(new Error('当前读取失败'))
      await settleComponent()
      assert.match(String(emptyTitle(render())), /当前读取失败/)
      old.resolve(undefined)
      await settleComponent()
      assert.match(String(emptyTitle(render())), /当前读取失败/, 'an old successful select cannot erase this connection error')
      const retry = render().find(({ type, props }) => type === 'Button' && typeof props.onPress === 'function')
      assert.ok(retry && typeof retry.props.onPress === 'function')
      shared.selectBot = async () => {}
      retry.props.onPress()
      render()
      await settleComponent()
      shared.conversationId = 'chat'
      let elements = render()
      assert.equal(elements.some(({ type }) => typeof type === 'function' && type.name === 'ContextRecords'), false, 'matching selection alone cannot expose a foreign payload')
      assert.doesNotMatch(componentText(elements), /秘密记录/)
      shared.data = parseConversation({ ...context, works: [parityWork()] })
      render()
      await settleComponent()
      elements = render()
      assert.match(componentText(elements), /当前连接尚未完成首读/)
      shared.synced = true
      shared.bots = []
      assert.match(String(emptyTitle(render())), /找不到这个 Bot/)
      shared.bots = [parityBot]
      shared.data = parseConversation(context)
      elements = render()
      if (route === 'work') assert.match(componentText(elements), /来源：共享历史中的工作记录/, 'a known work stays visible when Main no longer includes it')
      else assert.equal(elements.some(({ type }) => typeof type === 'function' && type.name === 'ContextRecords'), true)
      shared.connectionOnline = false
      elements = render()
      assert.match(componentText(elements) + String(emptyTitle(elements)), /状态未知/)
    } finally { app.close() }
  }
})


test('native context renders shared memory, external attribution, scheduler states and honest sent records', () => {
  const shared = parityShared()
  shared.data = parseConversation({ ...context,
    bot_settings: { skills: [], memory: [{ memory_id: 'm', text: '共享笔记', metadata: { epoch: 1, revision: 'a'.repeat(64), control_revision: 0, source_event_hash: 'b'.repeat(64), origin: 'agent', user_controlled: false, essential: true } }], memory_sync_conflicts: { items: [], has_more: false }, schedules: [] },
    scheduler: { scope_jobs: [], jobs: [{ scope: { kind: 'main' as const }, trigger: 'message:p', state: 'publishing' }, { scope: { kind: 'main' as const }, trigger: 'message:d', state: 'completed' }], unanswered: [] },
    sessions: [{ node: 'worker', agent: 'codex', session_id: 'external', cwd: '/shared', summary: '外部摘要', muted: true, created_at_ms: 1 }],
    sent: [{ message_id: 'sent', to: 'chat', text: '协作请求', created_at_ms: 2 }],
  })
  shared.nodes = [{ ...parityNode, online: false }]
  const app = componentHarness('../../../../../apps/mobile/app/b/[botId]/context.tsx', {
    'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) }, '@/data/live': { useLive: () => shared },
    '@vibeface/client/lib/node.ts': { messageOf, providerNames }, '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    '@vibeface/client/workspace/bot-settings.ts': { formatWhen: () => '刚才', scheduleFrequency: () => '一次' },
    '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, SettingsGroup: 'SettingsGroup', SettingsRow: 'SettingsRow', Details: 'Details' },
  })
  try {
    const elements = app.render('default')
    const records = elements.find(({ type }) => typeof type === 'function' && type.name === 'ContextRecords')
    assert.ok(records && typeof records.type === 'function')
    records.type(records.props)
    assert.equal(elements.some(({ props }) => props.label === 'Bot 自身会话' && String(props.description).includes('未知')), true)
    assert.equal(elements.some(({ props }) => String(props.description).includes('已静音 · 设备不可达')), true)
    assert.equal(elements.some(({ props }) => props.label === '后台工作历史 · 1'), true)
    const job = elements.find(({ type, props }) => typeof type === 'function' && type.name === 'JobObservation' && (props.job as { state: string }).state === 'publishing')
    assert.ok(job && typeof job.type === 'function')
    assert.match(componentText(job.type(job.props)), /回复同步中/)
    const message = elements.find(({ type }) => typeof type === 'function' && type.name === 'MessageRecord')
    assert.ok(message && typeof message.type === 'function')
    assert.match(componentText(message.type(message.props)), /对方是否收到或回复尚不清楚/)
    assert.equal(elements.some(({ props }) => props.href === '/b/bot'), true)
  } finally { app.close() }
})


test('native Stack focus restores non-chat views while the chat route owns its activation', () => {
  const views: string[] = []
  const stack = { Screen: 'Screen', Protected: 'Protected' }
  const app = componentHarness('../../../../../apps/mobile/app/_layout.tsx', {
    'expo-router': { Stack: stack, useRouter: () => ({}), usePathname: () => '/' }, 'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) }, 'expo-linking': { useLinkingURL: () => null },
    '@/data/live': { LiveWorkspace: 'LiveWorkspace', useLive: () => ({ setView: (view: string) => { views.push(view) } }), useWaiting: () => 0 },
    '@/data/context': { useWorkspace: () => ({ session: { phase: 'ready' } }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({}) },
    'uniwind': { useCSSVariable: () => ['white', 'black'] }, '@/lib/layout': { useWide: () => false }, 'react-native': { Platform: { OS: 'ios' }, View: 'View' },
  })
  try {
    const elements = app.render('default')
    const screens = elements.find(({ type }) => typeof type === 'function' && type.name === 'Screens')
    assert.ok(screens && typeof screens.type === 'function')
    screens.type(screens.props)
    const navigation = elements.find(({ type }) => type === stack)?.props.screenListeners
    assert.ok(typeof navigation === 'function')
    for (const name of ['b/[botId]/context', 'b/[botId]', 'b/[botId]/work/[workId]', 'tasks']) navigation({ route: { name } }).focus()
    assert.deepEqual(views, ['other', 'other', 'tasks'])
    assert.equal(elements.some(({ props }) => props.name === 'b/[botId]/context'), true)
  } finally { app.close() }
})


test("mobile task screen holds 待处理 and claims no emptiness while something waits", () => {
  let inbox: unknown[] = [{ kind: "question", done: false }, { kind: "confirm", check: { kind: "process" } }, { kind: "question", done: true }]
  const app = componentHarness("../../../../../apps/mobile/src/features/home/tasks.tsx", {
    "@/data/live": { useLive: () => null, useInbox: () => inbox },
    "@/data/context": { useWorkspace: () => ({ bots: [] }) },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
  })
  try {
    let elements = app.render("TasksScreen")
    const attention = () => elements.find(({ type }) => typeof type === "function" && type.name === "MobileMasthead")?.props.attention
    const empty = () => elements.some(({ type }) => typeof type === "function" && type.name === "MobileEmpty")
    assert.equal(attention(), null, "counts remain unknown without the live connection first read")
    assert.match(componentText(elements), /尚未读取任务记录/)
    assert.equal(elements.some(({ type }) => type === "PendingSection"), true)
    assert.equal(empty(), false)
    inbox = []
    elements = app.render("TasksScreen")
    assert.equal(attention(), null)
    assert.equal(empty(), true)
    assert.equal(elements.find(({ type }) => typeof type === "function" && type.name === "MobileEmpty")?.props.read, false)
  } finally { app.close() }
})


test("mobile task overview shows an unbound question summary and Bot entry", () => {
  const shared = parityShared()
  shared.openWorks = []
  shared.overview = workspaceModel.parseWorkspaceOverview({ bots: [parityBot], questions: [{
    question_id: "overview-question", conversation_id: "chat", bot: parityBot, text: "需要确认交付方式", severity: "attention", choices: [],
    request_id: null, work_id: null, about: { node: "worker", agent: "codex", session_id: "session", cwd: "/project", summary: "" }, created_at_ms: 1,
  }] })
  const app = componentHarness("../../../../../apps/mobile/src/features/home/tasks.tsx", {
    "@/data/live": { useLive: () => shared, useInbox: () => [] },
    "@/data/context": { useWorkspace: () => ({ bots: shared.bots }) },
    "@vibeface/client/identity/context.tsx": { useDevice: () => ({ directory: null }) },
    "@vibeface/client/workspace/labels.ts": taskLabels,
    "@vibeface/client/workspace/model.ts": workspaceModel,
    "@/lib/utils": { cn: (...values: unknown[]) => values.filter(Boolean).join(" ") },
  })
  try {
    const elements = app.render("TasksScreen")
    assert.equal(elements.some(({ type }) => typeof type === "function" && type.name === "MobileQuestionAttention"), true)
    assert.equal(nestedElements(elements, ["MobileQuestionAttention"]).some(({ type, props }) => type === "Link" && props.href === "/b/bot"), true)
  } finally { app.close() }
})


test("mobile questions send a choice on tap, preserve literal text, ignore on demand and retry only the unknown answer", async () => {
  for (const value of ['允许', '1', '１', '先核对配置\n确认后再继续', ignoredAnswer]) {
    const sent: unknown[][] = []
    let hold = pendingResult()
    const shared = { writable: true, answered: {} as Record<string, string>, pendingAnswers: [] as { question_id: string; text: string }[], openWorks: [], nodes: [] }
    const question = { question_id: 'mobile', text: '\n是否允许继续？\n线上结果未知。', choices: [{ label: '允许' }, { label: '先核对配置' }, { label: '停下' }, { label: '保持现状' }], severity: 'attention', about: { node: 'worker', session_id: 's', cwd: '' }, answer: null, closed: null }
    const app = componentHarness('../../../../../apps/mobile/src/features/chat/question-card.tsx', {
      '@/data/context': { useWorkspace: () => ({ members: [], bots: [], actions: { answer: (...args: unknown[]) => { sent.push(args); return hold.promise } } }) },
      '@/data/live': { useLive: () => shared },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: { entry: { device_id: 'gateway' }, principal: { device_id: 'mobile', source: 'membership', can_manage: false } } }) },
      '@vibeface/client/workspace/questions.ts': { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject },
      '@vibeface/client/lib/errors.ts': errors,
      'expo-haptics': { notificationAsync: async () => {}, NotificationFeedbackType: { Success: 'success' } },
      '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') },
    })
    const render = (props: object = {}) => app.render('QuestionCard', { question, ...props })
    const press = (label: string) => {
      const onPress = render().find(({ props }) => props.accessibilityLabel === label)?.props.onPress
      assert.ok(typeof onPress === 'function', `missing mobile control ${label}`)
      onPress()
    }
    const words = () => componentText(render())
    try {
      assert.deepEqual(render().filter(({ props }) => props.accessibilityRole === 'button' && props.onPress).map(({ props }) => props.accessibilityLabel), ['允许', '先核对配置', '停下', '保持现状'], 'one row per choice')
      const input = render().find(({ type }) => type === 'TextInput')
      assert.equal(input?.props.multiline, true)
      assert.equal(input?.props.submitBehavior, 'newline')
      assert.equal(input?.props.onSubmitEditing, undefined)
      const control = value === '允许' ? '允许' : value === ignoredAnswer ? '忽略' : '发送回答'
      if (control === '发送回答') {
        assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送回答')?.props.disabled, true, 'an empty answer cannot be sent')
        const write = input?.props.onChangeText
        assert.ok(typeof write === 'function')
        write(value)
        assert.equal(sent.length, 0, 'writing is local')
      }
      shared.writable = false
      press(control)
      assert.equal(sent.length, 0, 'read-only state also guards the handler')
      shared.writable = true
      press(control); press(control)
      assert.equal(sent.length, 1, 'the busy ref blocks a second tap before React rerenders')
      assert.equal(sent[0][1], value)
      shared.pendingAnswers = [{ question_id: 'mobile', text: value }]
      assert.match(words(), /正在发送/)
      assert.doesNotMatch(words(), /没确认送达/)
      hold.reject(new Error('connection lost')); await settleComponent()
      assert.match(words(), /没确认送达/)
      assert.equal(render().some(({ type, props }) => type === 'TextInput' || props.accessibilityLabel === '允许'), false)
      hold = pendingResult()
      const retry = render().find(({ type }) => type === 'Button')?.props.onPress
      assert.ok(typeof retry === 'function')
      retry(); retry()
      assert.equal(sent.length, 2)
      assert.equal(sent[1][1], value, 'retry preserves its original text')
      hold.resolve({ status: 'already_answered', text: '停下' }); await settleComponent()
      assert.match(words(), /已由另一端回答：停下/, 'the returned winner settles immediately without waiting for a snapshot')
      shared.answered = { mobile: '允许' }
      assert.ok(render({ question: { ...question, closed: 'request_lost', answer: { text: '停下' } } }).some(({ props }) => props.children === '已由另一端回答：停下'), 'standing answers precede closed and pending state')
    } finally { app.close() }
  }
})


test('Mobile useRun shares synchronous errors, explicit generation changes, and replacement action ownership', async () => {
  let generation = 1
  const app = componentHarness('../../../../../apps/mobile/src/lib/use-run.ts', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ connectionGeneration: () => generation }) },
  })
  const render = () => {
    app.render('useRun')
    return app.output as ReturnType<typeof import('@vibeface/client/lib/use-generation-action.ts').useGenerationAction>
  }
  try {
    await render().run('bad', () => { throw new Error('请填写设备编号。') })
    assert.equal(render().error, '请填写设备编号。')
    assert.equal(render().busy, null)

    for (const outcome of ['success', 'failure']) {
      const earlier = pendingResult(), newer = pendingResult()
      const operation = render().run('old', async ({ rebind }) => {
        await Promise.resolve()
        rebind({ from: generation, to: ++generation })
        await earlier.promise
      }, '旧操作成功。')
      await settleComponent()
      assert.equal(render().busy, 'old')
      await render().run('duplicate', () => assert.fail('the current claim must block a duplicate'))

      generation++
      assert.equal(render().busy, null)
      const runNew = render().run('new', () => newer.promise, '新操作成功。')
      if (outcome === 'success') earlier.resolve(undefined)
      else earlier.reject(new Error('旧操作失败。'))
      await operation
      assert.equal(render().busy, 'new')
      assert.equal(render().error, '')
      assert.equal(render().status, '')
      newer.resolve(undefined)
      await runNew
      assert.equal(render().busy, null)
      assert.equal(render().status, '新操作成功。')
    }

    const earlier = pendingResult(), newer = pendingResult()
    const operation = render().run('cancelled', () => earlier.promise, '已取消的操作成功。')
    render().cancel()
    const runNew = render().run('replacement', () => newer.promise)
    earlier.reject(new Error('已取消的操作失败。'))
    await operation
    assert.equal(render().busy, 'replacement')
    assert.equal(render().error, '')
    newer.resolve(undefined)
    await runNew
    assert.equal(render().busy, null)
  } finally { app.close() }
})


test('the Mobile join screen keeps the claimed invite through handoff and cancels only the preceding attempt', async () => {
  const path = '../../../../../apps/mobile/app/join.tsx'
  const firstLink = 'vibeface://join/first', nextLink = 'vibeface://join/next'
  let generation = 1, url = firstLink, session: { phase: string; code?: string } = { phase: 'unjoined' }
  const device = { connectionGeneration: () => generation, phase: 'disconnected', error: '', linkTarget: (value: string) => value.startsWith('vibeface://join/') ? 'current' : null, checkInvite: async () => 'open' }
  const attempts: { invitation: string; name: string; rebind: GenerationAction['rebind']; result: ReturnType<typeof pendingResult> }[] = []
  let cancelled = 0
  const actions = {
    join(invitation: string, name: string, rebind: GenerationAction['rebind']) {
      const result = pendingResult()
      attempts.push({ invitation, name, rebind, result })
      return result.promise
    },
    cancelJoin() { cancelled++; session = { phase: 'unjoined' }; generation++ },
  }
  const dependencies = {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    '@vibeface/client/identity/copy.ts': { joinCopy },
    '@vibeface/client/identity/recovery-link.ts': recoveryLinks,
    '@vibeface/client/lib/errors.ts': errors,
    'expo-linking': { useLinkingURL: () => url },
    '@/lib/join-link': { openedLink: (link: string) => link, switchedLink: () => null, keepSwitchedLink() {} },
    '@/data/context': { useWorkspace: () => ({ session, actions }) },
    '@/platform': { defaultName: () => '我的手机' },
    'react-native-reanimated': { default: { View: 'AnimatedView' }, __esModule: true },
  }
  const screen = componentHarness(path, dependencies, {}, 'spent', { '@/lib/use-run': '../../../../../apps/mobile/src/lib/use-run.ts' })
  const forms = { invite: componentHarness(path, dependencies, {}, 'JoinForm'), waiting: componentHarness(path, dependencies, {}, 'Waiting') }
  const render = () => {
    const tree = screen.render('default')
    const child = tree.find(({ type }) => typeof type === 'function' && type.name === 'JoinForm')
    return child ? forms.invite.render('JoinForm', child.props) : []
  }
  const button = () => render().find(({ type, props }) => type === 'Button' && [joinCopy.join, joinCopy.joining].includes(componentText(props.children)))?.props
  const press = () => { const handler = button()?.onPress; assert.ok(typeof handler === 'function'); handler() }
  try {
    render(); press(); press()
    assert.equal(attempts.length, 1)
    assert.equal(attempts[0].invitation, firstLink)
    assert.equal(attempts[0].name, '我的手机')
    attempts[0].rebind({ from: generation, to: ++generation })
    attempts[0].rebind({ from: generation, to: ++generation })
    assert.equal(button()?.disabled, true)
    session = { phase: 'joining', code: '123456' }
    render()
    assert.equal(screen.module.spent, firstLink)
    const child = screen.render('default').find(({ type }) => typeof type === 'function' && type.name === 'Waiting')
    assert.ok(child)
    const cancel = forms.waiting.render('Waiting', child.props).find(({ type, props }) => type === 'Button' && componentText(props.children) === joinCopy.stopWaiting)?.props.onPress
    assert.ok(typeof cancel === 'function'); cancel()
    assert.equal(cancelled, 1)

    forms.invite.close()
    forms.invite = componentHarness(path, dependencies, {}, 'JoinForm')
    url = nextLink; render(); press()
    assert.equal(attempts.length, 2)
    attempts[0].result.reject(new Error('旧邀请迟到失败。')); await settleComponent()
    assert.equal(button()?.disabled, true)
    assert.equal(render().some(({ props }) => props.error === '旧邀请迟到失败。'), false)
    attempts[1].result.reject(new Error('unexpected synthetic failure')); await settleComponent()
    assert.equal(button()?.disabled, false)
    assert.equal(screen.render('default').find(({ type }) => typeof type === 'function' && type.name === 'JoinForm')?.props.actionError, '这次加入没有成功。再试一次。')
  } finally { forms.invite.close(); forms.waiting.close(); screen.close() }
})


test('native chat folds runtime diagnostics and retains result checks without provider availability cards', async () => {
  let refreshed = 0
  const runtime = workspaceModel.parseTurnRuntime({ provider: 'codex', model: 'submitted-model', reasoning_effort: 'high', generation: 'generation', session_generation: 1, delivery_id: 'delivery', stage: 'record', execution: 'started', attempted_at_ms: 1, switched_from: null, switch_reason: null })
  const job: ManagerJob = { scope: { kind: 'main' }, trigger: 'message:one', message_id: 'one', reply_message_id: 'reply', state: 'completed', runtime }
  const shared = { ...parityShared(), refresh: async () => { refreshed++ }, nodes: [{ ...parityNode, capabilities: { ...parityNode.capabilities, manager_state: 'unverified' as const } }] }
  shared.data = parseConversation({ ...context, messages: [{ message_id: 'reply', event_hash: 'reply', role: 'assistant', text: '完成', created_at_ms: 2 }], scheduler: { scope_jobs: [], jobs: [job], unanswered: [] } })
  const extras = componentHarness('../../../../../apps/mobile/src/features/chat/work.tsx', {
    '@/data/live': { useLive: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/chat/protocol.ts': { promptIntent, canRetryJob, runtimeLabel }, '@vibeface/client/workspace/model.ts': workspaceModel,
    '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleTrigger }, '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/workspace/questions.ts': { questionOpen }, '@vibeface/client/lib/errors.ts': errors,
  })
  const details = componentHarness('../../../../../apps/mobile/src/components/kit.tsx', {
    'react-native-reanimated': { default: { View: 'AnimatedView' }, useAnimatedStyle: (read: () => unknown) => read(), useSharedValue: (value: number) => ({ value }), withTiming: (value: number) => value }, 'uniwind': { withUniwind: (value: unknown) => value },
    '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
  })
  const render = () => { extras.render('useChatExtras', 'chat'); return extras.output as { after: Map<string, { key: string; node: HarnessElement }[]>; cards: HarnessElement[] } }
  try {
    const result = render()
    assert.equal(botReach(shared.nodes), 'unverified')
    assert.equal(result.cards.length, 0, 'unverified readiness is sendable without a provider card')
    const row = result.after.get('reply')?.find((row) => row.key === 'runtime:message:one')?.node
    assert.ok(row)
    assert.equal(row.type, 'Details')
    assert.equal(row.props.label, '运行信息')
    details.render('Details', row.props)
    assert.doesNotMatch(componentText(details.output), /Codex|submitted-model|提交推理强度|记录结果/)
    const toggle = details.render('Details', row.props).find(({ props }) => props.accessibilityRole === 'button')
    assert.deepEqual(JSON.parse(JSON.stringify(toggle?.props.accessibilityState)), { expanded: false })
    assert.equal(typeof toggle?.props.onPress, 'function')
    if (typeof toggle?.props.onPress === 'function') toggle.props.onPress()
    details.render('Details', row.props)
    assert.match(componentText(details.output), /Codex · 提交模型 submitted-model · 提交推理强度 high · 记录结果/)
    for (const current of [{ ...job, state: 'reconciling' }, { ...job, state: 'failed', pending_check: true }] satisfies ManagerJob[]) {
      shared.data.scheduler!.jobs = [current]
      const cards = render().cards
      assert.equal(cards.length, 1)
      assert.equal(cards[0].props.title, '这次回复的结果待确认')
      const action = findActionProps(cards[0].props.actions, '再查一次')?.action
      assert.equal(typeof action, 'function')
      if (typeof action === 'function') await action()
    }
    assert.equal(refreshed, 2, 'result reconciliation remains a read-only action')
    shared.data.works = [parityWork({ state: 'unverified', outcome: 'unverified', failure_reason: '无法证明结果' })]
    shared.data.checks = [{ check_id: 'process', conversation_id: 'chat', trigger: 'message:one', attempt_id: 'attempt', detail: '进程还在运行', process_groups: [42], created_at_ms: 1 }]
    const cards = render().cards
    assert.equal(cards.filter(({ type }) => typeof type === 'function' && type.name === 'ConfirmCard').length, 2, 'unknown work and running-process checks stay visible')
  } finally { extras.close(); details.close() }
})


test('native composer adds photos and files without replacing current attachments or drafts', async (t) => {
  type Attachment = import('../data/types.ts').Attachment
  type NativeButton = { text: string; onPress?: () => void; style?: string }
  for (const os of ['ios', 'android']) await t.test(os, async () => {
    const original = { uri: 'file:///report.pdf', name: 'report.pdf', size: 20, mimeType: 'application/pdf' }
    const current = { uri: 'file:///current.txt', name: 'current.txt', size: null, mimeType: null }
    const cache = new Map([
      ['file:///cache/original.heic', { name: 'original.heic', size: 30, type: 'image/heic' }],
      ['file:///cache/transparent.png', { name: 'transparent.png', size: 40, type: 'image/png' }],
      ['file:///cache/photo.jpg', { name: 'photo.jpg', size: 50, type: 'image/jpeg' }],
    ])
    let files: Attachment[] = [original], updates = 0, renders = 0, usedCallback = 0
    let photos = pendingResult(), documents = pendingResult(), selected: (() => void) | undefined
    const photoCalls: unknown[] = [], fileCalls: unknown[] = []
    const alerts: { title: string; message?: string; buttons?: NativeButton[] }[] = []
    let sheet: { options: string[]; cancelButtonIndex: number } | undefined, sheetSelect: ((index: number) => void) | undefined
    const app = componentHarness('../../../../../apps/mobile/src/features/chat/composer.tsx', {
      'react-native': {
        Platform: { OS: os }, TextInput: 'TextInput', View: 'View',
        ActionSheetIOS: { showActionSheetWithOptions(options: typeof sheet, select: typeof sheetSelect) { sheet = options; sheetSelect = select } },
        Alert: { alert(title: string, message?: string, buttons?: NativeButton[]) { alerts.push({ title, message, buttons }) } },
      },
      'expo-image-picker': {
        UIImagePickerPreferredAssetRepresentationMode: { Current: 'current' },
        launchImageLibraryAsync(options: unknown) { photoCalls.push(options); return photos.promise },
      },
      'expo-document-picker': { getDocumentAsync(options: unknown) { fileCalls.push(options); return documents.promise } },
      'expo-file-system': { File: class {
        readonly name: string; readonly size: number; readonly type: string
        constructor(uri: string) {
          const file = cache.get(uri)
          assert.ok(file)
          this.name = file.name; this.size = file.size; this.type = file.type
        }
      } },
    })
    const render = () => {
      const callback = ++renders
      return app.render('Composer', {
        chrome: { kind: 'solid' }, value: '保留草稿', onChange() {}, files, attachmentScope: JSON.stringify(['network', 'chat', ['bot_main', 'bot'], 'continue']),
        onFiles(next: Attachment[]) { usedCallback = callback; updates++; files = next }, onPickFiles(added: import('../data/types.ts').PickedAttachment[]) { usedCallback = callback; updates++; files = [...files, ...added] }, blocked: null, sending: false, onSend() {},
      })
    }
    const choose = (index: number) => {
      const attach = render().find(({ props }) => String(props.accessibilityLabel).startsWith('添加附件'))?.props.onPress
      assert.equal(typeof attach, 'function')
      if (typeof attach === 'function') attach()
      if (os === 'ios') {
        assert.deepEqual(JSON.parse(JSON.stringify(sheet)), { options: ['照片图库', '文件', '取消'], cancelButtonIndex: 2 })
        selected = () => sheetSelect?.(index)
      } else {
        const menu = alerts.at(-1)
        assert.ok(menu)
        assert.equal(menu?.title, '添加附件')
        assert.deepEqual(Array.from(menu.buttons ?? [], ({ text }) => text), ['照片图库', '文件', '取消'])
        selected = menu.buttons?.[index]?.onPress
      }
      selected?.()
    }
    const snapshot = () => JSON.parse(JSON.stringify(files))
    try {
      choose(0)
      selected?.()
      assert.equal(photoCalls.length, 1, 'only one picker can run at once')
      assert.equal(fileCalls.length, 0)
      assert.deepEqual(JSON.parse(JSON.stringify(photoCalls[0])), { mediaTypes: ['images'], allowsMultipleSelection: true, quality: 1, preferredAssetRepresentationMode: 'current' })
      files = [current]; render()
      photos.resolve({ canceled: false, assets: [
        { uri: 'file:///cache/original.heic', fileName: null },
        { uri: 'file:///cache/transparent.png' },
        { uri: 'file:///cache/photo.jpg', fileName: 'holiday.jpg', fileSize: 50, mimeType: 'image/jpeg' },
      ] })
      await settleComponent()
      assert.equal(usedCallback, renders, 'completion uses the current draft setter')
      assert.deepEqual(snapshot(), [current,
        { uri: 'file:///cache/original.heic', name: 'original.heic', size: 30, mimeType: 'image/heic' },
        { uri: 'file:///cache/transparent.png', name: 'transparent.png', size: 40, mimeType: 'image/png' },
        { uri: 'file:///cache/photo.jpg', name: 'holiday.jpg', size: 50, mimeType: 'image/jpeg' },
      ])
      assert.equal(render().find(({ type }) => type === 'TextInput')?.props.value, '保留草稿')
      const selectedFiles = snapshot()
      photos = pendingResult(); choose(0); photos.resolve({ canceled: true, assets: null }); await settleComponent()
      choose(2)
      assert.equal(updates, 1, 'picker cancellation and menu cancellation leave the draft alone')
      assert.deepEqual(snapshot(), selectedFiles)
      photos = pendingResult(); choose(0); photos.reject(new Error('无法读取所选照片')); await settleComponent()
      assert.equal(alerts.at(-1)?.title, '未能添加附件')
      assert.equal(alerts.at(-1)?.message, '无法读取所选照片')
      assert.deepEqual(snapshot(), selectedFiles)
      photos = pendingResult(); choose(0); photos.reject(new Error('照片权限已拒绝')); await settleComponent()
      assert.equal(alerts.at(-1)?.message, '照片权限已拒绝')
      assert.deepEqual(snapshot(), selectedFiles, 'permission denial preserves the draft attachments')
      choose(1)
      assert.equal(fileCalls.length, 1, 'an error releases the picker for another attempt')
      assert.deepEqual(JSON.parse(JSON.stringify(fileCalls[0])), { multiple: true, copyToCacheDirectory: true })
      documents.resolve({ canceled: false, assets: [
        { uri: 'file:///report.pdf', name: 'report.pdf', size: 20, mimeType: 'application/pdf' },
        { uri: 'file:///notes.txt', name: 'notes.txt' },
      ] })
      await settleComponent()
      assert.deepEqual(snapshot(), [...selectedFiles, original, { uri: 'file:///notes.txt', name: 'notes.txt', size: null, mimeType: null }])
      documents = pendingResult(); choose(1); documents.resolve({ canceled: true, assets: null }); await settleComponent()
      assert.equal(updates, 2)
      const remove = render().find(({ props }) => props.accessibilityLabel === '移除 transparent.png')?.props.onPress
      assert.equal(typeof remove, 'function')
      if (typeof remove === 'function') remove()
      assert.equal(files.some(({ name }) => name === 'transparent.png'), false)
      assert.equal(files.some(({ name }) => name === 'holiday.jpg'), true)
    } finally { app.close() }
  })
})


test('native attachment results cannot cross a draft scope or survive its disabled or unmounted lifetime', async (t) => {
  type NativeButton = { text: string; onPress?: () => void }
  for (const os of ['ios', 'android']) for (const source of ['photos', 'files']) for (const change of ['bot', 'bot-roundtrip', 'network', 'blocked', 'blocked-roundtrip', 'sending', 'unmounted', 'stale-menu', 'blocked-before-menu', 'sending-before-menu']) await t.test(`${os} ${source} ${change}`, async () => {
    const live = parityShared()
    const conversation: import('../data/types.ts').Conversation = {
      origin: { kind: 'network' }, messages: [], before: new Map(), rest: [], answers: new Map(), hidden: new Set(), unread: null,
      works: [parityWork()], replying: false, status: null, stoppable: false, blocked: null,
    }
    const workspace = { get networkId() { return device.ready.principal.network_id }, deviceId: 'phone', bots: [parityBot, { ...parityBot, bot_id: 'other', conversation_id: 'other-chat' }], conversations: { chat: conversation, 'other-chat': { ...conversation } }, members: [], primary: null, actions: { open() {} }, connection: { entry: '工作设备' } }
    const device = { link: { kind: 'online' }, ready: { entry: { device_id: 'gateway' }, principal: { network_id: 'network', can_manage: true } } }
    let botId = 'bot', sending = false, calls = 0, selected: (() => void) | undefined
    const result = pendingResult()
    const screen = nativeChatHarness(os, {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId }) },
      '@/data/context': { useWorkspace: () => workspace }, '@/data/live': { useLive: () => live }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    })
    const composer = componentHarness('../../../../../apps/mobile/src/features/chat/composer.tsx', {
      'react-native': {
        Platform: { OS: os }, TextInput: 'TextInput', View: 'View',
        ActionSheetIOS: { showActionSheetWithOptions(_options: unknown, select: (index: number) => void) { selected = () => select(source === 'photos' ? 0 : 1) } },
        Alert: { alert(_title: string, _message?: string, buttons?: NativeButton[]) { selected = buttons?.[source === 'photos' ? 0 : 1]?.onPress } },
      },
      'expo-image-picker': { UIImagePickerPreferredAssetRepresentationMode: { Current: 'current' }, launchImageLibraryAsync() { calls++; return result.promise } },
      'expo-document-picker': { getDocumentAsync() { calls++; return result.promise } },
      'expo-file-system': { File: class { name = 'photo.jpg'; size = 20; type = 'image/jpeg' } },
    })
    const props = () => {
      screen.render('default')
      const current = screen.render('default').find(({ type }) => type === 'Composer')?.props
      assert.ok(current)
      return current
    }
    const render = () => composer.render('Composer', { ...props(), sending })
    try {
      props().onChange?.('A 草稿')
      const original = [{ uri: 'file:///a.txt', name: 'a.txt', size: 1, mimeType: 'text/plain' }]
      const onFiles = props().onFiles; assert.ok(typeof onFiles === 'function'); await onFiles(original)
      const scope = props().attachmentScope
      assert.equal(typeof scope, 'string')
      if (change === 'blocked-before-menu') conversation.blocked = '当前设备仅可查看'
      if (change === 'sending-before-menu') sending = true
      const attach = render().find(({ props }) => String(props.accessibilityLabel).startsWith('添加附件'))?.props.onPress
      assert.ok(typeof attach === 'function'); attach()
      if (change.endsWith('-before-menu')) assert.equal(calls, 0, 'a disabled composer cannot open a picker even when its handler is invoked')
      else if (change !== 'stale-menu') { assert.ok(selected); selected(); assert.equal(calls, 1) }
      if (change === 'bot' || change === 'bot-roundtrip' || change === 'stale-menu') {
        botId = 'other'; render()
        props().onChange?.('B 草稿')
        const setFiles = props().onFiles; assert.ok(typeof setFiles === 'function')
        await setFiles([{ uri: 'file:///b.txt', name: 'b.txt', size: 2, mimeType: 'text/plain' }]); render()
        if (change === 'bot-roundtrip') { botId = 'bot'; render(); assert.equal(props().attachmentScope, scope) }
      }
      if (change === 'network') { device.ready.principal.network_id = 'other-network'; render() }
      if (change === 'blocked' || change === 'blocked-roundtrip') { conversation.blocked = '当前设备仅可查看'; render(); if (change === 'blocked-roundtrip') { conversation.blocked = null; render() } }
      if (change === 'sending') { sending = true; render() }
      if (['bot', 'network', 'stale-menu'].includes(change)) assert.notEqual(props().attachmentScope, scope)
      if (change === 'unmounted') composer.close()
      if (change === 'stale-menu') { assert.ok(selected); selected(); assert.equal(calls, 0, 'a menu opened in A cannot start a picker in B') }
      if (change === 'blocked' || change === 'sending') {
        const button = render().find(({ props }) => String(props.accessibilityLabel).startsWith('添加附件'))?.props
        assert.equal(button?.disabled, true)
        assert.ok(typeof button?.onPress === 'function'); button.onPress(); assert.equal(calls, 1)
      }
      const before = JSON.stringify(props().files), text = props().value
      result.resolve({ canceled: false, assets: [{ uri: 'file:///photo.jpg', name: 'photo.jpg', fileName: 'photo.jpg', size: 20, mimeType: 'image/jpeg' }] })
      await settleComponent()
      assert.equal(JSON.stringify(props().files), before, 'late completion leaves the actual bound draft setter untouched')
      assert.equal(props().value, text)
      botId = 'bot'; device.ready.principal.network_id = 'network'
      assert.equal(props().value, 'A 草稿')
      assert.equal(JSON.stringify(props().files), JSON.stringify(original), 'the original draft also retains its attachments')
    } finally { composer.close(); screen.close() }
  })
})


test('native composer preserves local input and visible blockers with compact chat chrome on iOS and Android', () => {
  for (const os of ['ios', 'android']) {
    let retried = 0
    const live = { ...parityShared(), nodes: parseNodes({ nodes: [{ ...parityNode, capabilities: { ...parityNode.capabilities, manager_state: 'unverified' } }] }) }
    const conversation: import('../data/types.ts').Conversation = {
      origin: { kind: 'network' }, messages: [{ message_id: 'one', event_hash: 'one', role: 'user', text: 'hello', created_at_ms: 1 }], before: new Map(), rest: [], answers: new Map(), hidden: new Set(), unread: null,
      works: [], replying: false, status: null, stoppable: false, blocked: null,
    }
    const workspace = { networkId: 'network', deviceId: 'phone', bots: [parityBot], conversations: { chat: conversation }, members: [], primary: null, actions: { open() {} }, connection: { entry: '工作设备' } }
    const device = { link: { kind: 'online' }, ready: { entry: { device_id: 'gateway' }, principal: { can_manage: true } }, retryNow: () => { retried++ } }
    const screen = nativeChatHarness(os, {
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({ botId: 'bot' }) },
      '@/data/context': { useWorkspace: () => workspace }, '@/data/live': { useLive: () => live }, '@vibeface/client/identity/context.tsx': { useDevice: () => device },
      '@/features/chat/chrome': { useChatChrome: () => ({ kind: 'solid' }) },
    })
    const composer = componentHarness('../../../../../apps/mobile/src/features/chat/composer.tsx', { 'react-native': { Platform: { OS: os }, TextInput: 'TextInput', View: 'View' } })
    const offline = componentHarness('../../../../../apps/mobile/src/components/connection-pill.tsx', {
      '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@/data/context': { useWorkspace: () => workspace },
      '@vibeface/client/identity/copy.ts': { offlineCopy }, '@vibeface/client/workspace/home.ts': { unreachable }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    })
    const props = () => screen.render('default').find(({ type }) => type === 'Composer')!.props
    const render = () => composer.render('Composer', { ...props(), value: 'draft' })
    try {
      assert.equal(props().blocked, null)
      assert.equal(componentText(props().children), '', 'unverified readiness adds no availability prompt')
      assert.equal(props().sending, false, 'network activity does not lock local saving')
      assert.equal(screen.render('default').filter(({ type }) => type === 'OfflineBar').length, 0)
      assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, false)
      device.link.kind = 'offline'
      assert.equal(screen.render('default').filter(({ type }) => type === 'OfflineBar').length, 1)
      assert.equal(props().blocked, null, 'a disconnected transport still permits durable local input')
      assert.equal(render().find(({ type }) => type === 'TextInput')?.props.placeholder, '发消息')
      const recovery = offline.render('OfflineBar', { chat: true })
      assert.equal(recovery.filter(({ props }) => props.role === 'status').length, 1)
      const retry = recovery.find(({ type }) => type === 'Button')?.props.onPress
      assert.equal(typeof retry, 'function')
      if (typeof retry === 'function') retry()
      assert.equal(retried, 1)
      assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, false)
      device.link.kind = 'online'
      for (const state of ['needs_setup', 'none', 'offline'] as const) {
        live.nodes = state === 'none' ? [] : parseNodes({ nodes: [{ ...parityNode, online: state !== 'offline', capabilities: { ...parityNode.capabilities, manager_state: 'needs_setup' } }] })
        const executionHint = composerHint({ hasConversation: true, loaded: true, bot: botReach(live.nodes) })
        assert.ok(executionHint)
        const elements = render()
        assert.equal(props().blocked, null, 'execution readiness does not block local saving')
        assert.equal(elements.find(({ type }) => type === 'TextInput')?.props.placeholder, '发消息')
        assert.equal(elements.find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, false)
        assert.equal(componentText(composer.output).split(executionHint).length - 1, 1)
        const links = screen.render('default').filter(({ type, props }) => type === 'Link' && componentText(props.children).includes(state === 'offline' ? '查看设备' : '去设置'))
        assert.equal(links.filter(({ props }) => props.href === (state === 'none' ? '/ai' : '/device/worker')).length, 1)
      }
      conversation.blocked = '当前设备仅可查看'
      assert.equal(render().find(({ type }) => type === 'TextInput')?.props.placeholder, conversation.blocked)
      assert.equal(componentText(composer.output).split(conversation.blocked).length - 1, 1, 'the write blocker remains visible beside any actionable children')
      assert.equal(render().find(({ props }) => props.accessibilityLabel === '发送')?.props.disabled, true)
    } finally { screen.close(); composer.close(); offline.close() }
  }
})


test('native private files preserve sealed records and external originals while erase fences delayed pickers and retries failed deletes', async () => {
  const fixture = nativeFileFixture()
  const app = componentHarness(new URL('./files.ts', import.meta.url).href, { 'expo-file-system': fixture.native, 'react-native-mmkv': fixture.mmkv, '@vibeface/client/lib/node.ts': nodeSettings, './legacy-recovery': { isLegacyRecoveryFile } }, { crypto, TextDecoder, Uint8Array })
  const { privateFiles } = app.module as typeof import('./files.ts')
  const source = new fixture.native.File('file:///provider/original.json'), exported = new fixture.native.File('file:///export/kept.json')
  source.writeSync('original'); exported.writeSync('external export')
  const envelope = JSON.stringify({ version: 1, network_id: 'a'.repeat(64), envelope: { header: { format: 'vibeface-recovery', version: 1, kdf: 'argon2id', memory_kib: 19456, iterations: 2, parallelism: 1, salt: '1'.repeat(32), cipher: 'xchacha20poly1305', nonce: '2'.repeat(48) }, ciphertext: '3'.repeat(64) } })
  const earlier = new fixture.native.File('file:///documents/vibeface-recovery-ABCDEF12-2026-11-08.json')
  const similar = new fixture.native.File('file:///documents/vibeface-recovery-ABCDEF12-2026-11-08-notes.json')
  const nonRecovery = new fixture.native.File('file:///documents/vibeface-recovery-ABCDEF13-2026-11-08.json')
  const externalRecovery = new fixture.native.File('file:///export/vibeface-recovery-ABCDEF12-2026-11-08.json')
  earlier.writeSync(envelope); similar.writeSync(envelope); nonRecovery.writeSync('{"user":"notes"}'); externalRecovery.writeSync(envelope)
  privateFiles.recoveryStorage.write('sealed copy with index')
  const reloaded = componentHarness(new URL('./files.ts', import.meta.url).href, { 'expo-file-system': fixture.native, 'react-native-mmkv': fixture.mmkv, '@vibeface/client/lib/node.ts': nodeSettings, './legacy-recovery': { isLegacyRecoveryFile } }, { crypto, TextDecoder, Uint8Array })
  assert.equal((reloaded.module as typeof import('./files.ts')).privateFiles.recoveryStorage.read(), 'sealed copy with index')
  fixture.fail('recovery-write')
  assert.throws(() => privateFiles.recoveryStorage.write('not committed'), /controlled write failure/)
  assert.equal(privateFiles.recoveryStorage.read(), 'sealed copy with index')
  fixture.fail('recovery-index')
  assert.throws(() => privateFiles.recoveryStorage.write('not indexed'), /controlled index failure/)
  assert.equal(privateFiles.recoveryStorage.read(), 'sealed copy with index')
  fixture.fail('')
  const picked = await privateFiles.run(() => privateFiles.retain(source.uri))
  assert.notEqual(picked.uri, source.uri)
  const nativeCopy = new fixture.native.File('file:///cache/DocumentPicker/copied.json')
  nativeCopy.writeSync('picker cache')
  fixture.fail(nativeCopy.uri)
  assert.throws(() => privateFiles.releasePicker(nativeCopy.uri), /controlled delete failure/)
  await assert.rejects(privateFiles.erase(async () => {}), /controlled delete failure/)
  assert.equal(picked.exists, true, 'failed cleanup retains retry evidence and owned copies')
  fixture.fail('')
  const delayed = pendingResult()
  let returned = false, erased = false
  const work = privateFiles.run(async current => {
    await delayed.promise
    nativeCopy.writeSync('late picker cache')
    try { if (current()) { const file = await privateFiles.retain(nativeCopy.uri); if (current()) returned = true; else privateFiles.release(file.uri) } }
    finally { privateFiles.releasePicker(nativeCopy.uri) }
  })
  const rejected = assert.rejects(work, /正在清除/)
  const erase = privateFiles.erase(async () => { erased = true })
  await settleComponent()
  assert.equal(erased, false)
  await assert.rejects(privateFiles.run(async () => {}), /正在清除/)
  delayed.resolve(undefined)
  await Promise.all([erase, rejected])
  assert.equal(returned, false)
  assert.equal(erased, true)
  assert.equal(nativeCopy.exists, false)
  assert.equal(picked.exists, false)
  assert.equal(privateFiles.recoveryStorage.read(), null)
  assert.equal(source.textSync(), 'original')
  assert.equal(exported.textSync(), 'external export')
  assert.equal(earlier.exists, false, 'the earlier writer-owned root copy is erased')
  assert.equal(similar.textSync(), envelope, 'a similar user filename is preserved')
  assert.equal(nonRecovery.textSync(), '{"user":"notes"}', 'a matching name without a sealed recovery shape is preserved')
  assert.equal(externalRecovery.textSync(), envelope, 'an external same-name exported recovery is preserved')
})


test('native system notification events keep typed destinations and clear navigation on erase', async () => {
  const fixture = nativeFileFixture()
  const owned = componentHarness(new URL('./files.ts', import.meta.url).href, { 'expo-file-system': fixture.native, 'react-native-mmkv': fixture.mmkv, '@vibeface/client/lib/node.ts': nodeSettings, './legacy-recovery': { isLegacyRecoveryFile } }, { crypto, TextDecoder, Uint8Array })
  const { privateFiles } = owned.module as typeof import('./files.ts')
  type Response = { notification: { date: number; request: { identifier: string; content: { data: { destination: NotificationDestination } } } }; actionIdentifier: string }
  let response: ((value: Response) => void) | undefined
  let dismissed = 0, forgotten = 0
  const storage = new Map<string, string>()
  const scheduled: unknown[] = []
  const app = componentHarness(new URL('./index.ts', import.meta.url).href, {
    'react-native': { Platform: { OS: 'ios', isPad: false }, AppState: { currentState: 'active', addEventListener: () => ({ remove() {} }) } },
    'expo-file-system': fixture.native,
    'expo-notifications': {
      setNotificationHandler() {}, AndroidImportance: { HIGH: 1 }, setNotificationChannelAsync: async () => {},
      addNotificationResponseReceivedListener(listener: typeof response) { response = listener; return { remove() { response = undefined } } },
      getLastNotificationResponseAsync: async () => null, clearLastNotificationResponseAsync: async () => {},
      scheduleNotificationAsync: async (value: unknown) => { scheduled.push(value); return 'scheduled' },
      dismissAllNotificationsAsync: async () => { dismissed++ },
    },
    'react-native-mmkv': { createMMKV: () => ({ getString: (key: string) => storage.get(key), set: (key: string, value: string) => storage.set(key, value), clearAll: () => storage.clear(), getAllKeys: () => [...storage.keys()] }) },
    '@vibeface/secure-native': { lan: {}, secure: {} },
    '@vibeface/client/lib/transport.ts': { createTransport: () => ({ forgetClient: async () => { forgotten++ } }) },
    '@vibeface/client/identity/copy.ts': { lostKeyText: () => '' },
    './files': { privateFiles },
    './notification-opens': { notificationOpenings }, './outbox': { mobileOutbox },
  })
  const { mobilePlatform } = app.module as typeof import('./index.ts')
  const destination: NotificationDestination = { network_id: 'network-fixture', device_id: 'device-fixture', notification_id: 'notification-fixture', conversation_id: 'conversation-fixture' }
  const opened: NotificationDestination[] = []
  const unwatch = mobilePlatform.notifier.watchOpen?.(async value => { opened.push(value); return true })
  await mobilePlatform.notifier.show({ title: 'fixture', body: 'fixture', tag: 'same', destination }, () => {})
  await privateFiles.run(async () => {})
  const event: Response = { notification: { date: 1, request: { identifier: 'same', content: { data: { destination } } } }, actionIdentifier: 'open' }
  response?.(event); response?.(event)
  await new Promise<void>(resolve => setImmediate(resolve))
  assert.deepEqual(opened, [destination])
  assert.deepEqual(JSON.parse(JSON.stringify(scheduled)), [{ identifier: 'same', content: { title: 'fixture', body: 'fixture', data: { destination } }, trigger: { channelId: 'vibeface-silent' } }])
  await mobilePlatform.erase?.()
  response?.({ ...event, notification: { ...event.notification, date: 2 } })
  await new Promise<void>(resolve => setImmediate(resolve))
  assert.deepEqual(opened, [destination], 'erase removes the navigation consumer')
  assert.equal(dismissed, 1)
  assert.equal(forgotten, 1)
  unwatch?.()
})


test('the native switch confirmation fixes the decoded entry and never erases for a changed link or cancellation', async () => {
  const entryId = 'a'.repeat(64), link = 'vibeface://join/vibeface1fixture'
  let url = link, erased = 0, canceled = 0
  const kept: (string | null)[] = [], failures: unknown[] = []
  const app = componentHarness('../../../../../apps/mobile/app/switch-network.tsx', {
    'expo-router': { router: { back() { throw new Error('module router is unbound') } }, useRouter: () => ({ back: () => { canceled++ } }), useLocalSearchParams: () => ({ link }) },
    'expo-linking': { useLinkingURL: () => url },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: { members: [] }, linkTarget: () => 'other' }) },
    '@vibeface/client/identity/copy.ts': { soleManager: () => false, switchCopy: { title: '换到新的网络？', erase: () => '清除应用内数据', confirm: '换网络' } },
    '@vibeface/client/identity/switch-network.ts': { joinDestination },
    '@/platform': { mobilePlatform: { secure: { ticketDecode: () => JSON.stringify({ entry_id: entryId }) } }, self: '这台手机' },
    '@/lib/join-link': { openedLink: (value: string | null) => value?.startsWith('vibeface://join/') && joinTicket(value) ? value : null, keepSwitchedLink: (value: string | null) => kept.push(value) },
    '@/lib/factory-reset': { factoryReset: async () => { erased++ } },
    '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_name: string, action: () => Promise<void>) => { try { await action() } catch (cause) { failures.push(cause) } } }) },
  })
  try {
    app.render('default')
    assert.match(componentText(app.output), new RegExp(entryId))
    const cancel = app.render('default').find(({ props }) => props.variant === 'ghost')?.props.onPress
    assert.ok(typeof cancel === 'function'); cancel()
    assert.equal(canceled, 1); assert.equal(erased, 0)
    url = 'vibeface://join/vibeface1different'
    const changed = app.render('default').find(({ props }) => props.variant === 'danger')?.props
    assert.equal(changed?.disabled, true)
    assert.ok(typeof changed?.onPress === 'function'); changed.onPress(); await settleComponent()
    assert.equal(erased, 0); assert.equal(failures.length, 1)
    url = link
    const confirmed = app.render('default').find(({ props }) => props.variant === 'danger')?.props
    assert.equal(confirmed?.disabled, false)
    assert.ok(typeof confirmed?.onPress === 'function'); confirmed.onPress(); await settleComponent()
    assert.equal(erased, 1); assert.deepEqual(kept, [link])
  } finally { app.close() }
})
