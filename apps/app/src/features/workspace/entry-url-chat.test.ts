import * as deliveryModel from '@vibeface/client/delivery/model.ts'
import { type DeliveryBudget } from '@vibeface/client/delivery/types.ts'
import { selectionOwner } from '@vibeface/client/lib/selection.ts'

import assert from "node:assert/strict"
import test from "node:test"
import { entryAddress, messageOf, providerNames } from "@vibeface/client/lib/node.ts"


import type { InputOptions, InputTarget, PendingMessage, WorkRecord, WorkspaceDomainState, WorkspaceFilter } from "@vibeface/client/workspace/types.ts"
import { parseConversation, parseBot } from "@vibeface/client/workspace/model.ts"
import { botCopy, folderLabels, placeChanges, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import * as errors from "@vibeface/client/lib/errors.ts"
import { inputTargetKey, promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { setUpOnManager } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread, taskMessage } from "@vibeface/client/chat/timeline.ts"
import { accessOf } from "@vibeface/client/identity/access.ts"
import { networkKey } from "@vibeface/client/workspace/storage.ts"
import { byRecent, listTime, mainConversationAttention } from "@vibeface/client/workspace/home.ts"
import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"

import * as systemNotifications from '@vibeface/client/workspace/system-notifications.ts'

import { context, componentHarness, pendingResult, nestedElements, componentText, conversationHarness, parityBot, parityWork, parityShared, settleComponent } from './entry-url-test-kit.test-support.ts'
import * as lastSeenModel from './test-last-seen.test-support.ts'


test('Web and Mobile main Bot callers agree on unread while Web shows the session preview and Mobile counts task push badges separately', () => {
  const target = { kind: 'bot_main', bot_id: parityBot.bot_id }
  const notifications = workspaceModel.parseNotifications({ notifications: [
    { notification_id: 'main', conversation_id: 'chat', kind: 'manager_response', target, text: '主聊天回复' },
    { notification_id: 'session', conversation_id: 'chat', kind: 'session_attributed', target, text: '会话提醒' },
    { notification_id: 'task', conversation_id: 'chat', kind: 'manager_response', target: { kind: 'task', task_id: 'task' }, text: '任务回复' },
  ] })
  const shared = { ...parityShared(), data: null, notifications }
  const web = componentHarness('./conversation-list.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
    '@vibeface/client/workspace/home.ts': { byRecent, listTime, mainConversationAttention },
    '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle' },
    './workspace-shell': { useCreateBot: () => () => {} },
  })
  const storage = new Map<string, string>()
  const keyValue = { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => { storage.set(key, value) } }
  systemNotifications.notificationPreferences(keyValue, 'network').write({ enabled: true, badge: true, sound: false, mutedConversations: [] })
  const device = { ready: { principal: { network_id: 'network' }, entry: { kind: 'gateway', address: 'https://node.example', device_id: 'gateway' }, info: { name: '工作设备' }, isCurrent: () => true }, phase: 'ready', link: { kind: 'online' }, entries: [], directory: null }
  const appBadges: number[] = []
  const mobile = componentHarness('../../../../../apps/mobile/src/data/live.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device },
    '@vibeface/client/identity/access.ts': { accessOf },
    '@vibeface/client/lib/node.ts': { entryAddress },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared },
    '@vibeface/client/workspace/home.ts': { mainConversationAttention },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@/platform': { mobilePlatform: { storage: keyValue, onForeground: () => () => {} } },
    '@/platform/remote-notifications': { remoteNotifications: { sync: async () => {} } },
    'expo-notifications': { setBadgeCountAsync: async (count: number) => { appBadges.push(count) }, addPushTokenListener: () => ({ remove() {} }) },
    './context': { WorkspaceProvider: 'NativeWorkspace' }, './last-seen': lastSeenModel,
  }, {}, 'Live')
  try {
    const webRow = web.render('ConversationList').find(({ type }) => typeof type === 'function' && type.name === 'Row')
    assert.equal(webRow?.props.preview, '会话提醒')
    assert.equal(webRow?.props.unread, 1)
    const mobileWorkspace = workspaceModel.record(mobile.render('Live').find(({ type }) => type === 'NativeWorkspace')?.props.value)
    assert.equal(workspaceModel.record(mobileWorkspace.unread).chat, webRow?.props.unread)
    assert.deepEqual(appBadges, [2])
  } finally {
    web.close()
    mobile.close()
  }
})


test('both pending input controls retry only a matching safe refusal and keep all scope targets', async () => {
  const targets: InputTarget[] = [{ kind: 'bot_main', bot_id: 'bot' }, { kind: 'task', task_id: 'task' }, { kind: 'channel_task', channel_id: 'channel', task_id: 'task', bot_id: 'bot' }, { kind: 'work', work_id: 'work' }, { kind: 'routine', bot_id: 'bot', trigger: 'schedule:one' }, { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }]
  for (const platform of ['web', 'native']) for (const target of targets) {
    let pending: PendingMessage = { report_results: true, message_id: 'original', conversation_id: 'chat', target, intent: 'continue', text: '保留原交代', sequence: 1, created_at_ms: 1, delivery: 'unknown', attachments: [{ hash: 'a'.repeat(64), size: 3, name: 'notes.txt' }] }
    const retries: string[] = [], closed: string[] = [], confirmed: [string, string][] = []
    const shared = { ...parityShared(), info: { device_id: 'original-entry', name: '本人电脑' }, workspaceState: { channels: [], tasks: [], assignments: [], inputs: [] as WorkspaceDomainState['inputs'], coverage: { limit: 128, truncated: false } },
      confirmInputEntry: async (messageId: string, entry: string) => { confirmed.push([messageId, entry]); pending = { ...pending, entry_device_id: entry } },
      queryInput: async () => { pending = { ...pending, delivery: 'accepted', receipt: { message_id: pending.message_id, target, stage: 'refused', retry_safe: true, control_event: 'c'.repeat(64), actual_turn_id: null, reason: '可以重试原输入' } } },
      retryPendingMessage: async (id: string) => { retries.push(id) },
      releaseMessagePending: async (id: string) => { closed.push(id) },
      sendMessage: async () => { throw new Error('must not create another input') },
    }
    const app = componentHarness(platform === 'web' ? './workspace-chat.tsx' : '../../../../../apps/mobile/src/features/workspace/workspace.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared }, '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, action: () => Promise<void>) => action() }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
      '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_key: string, action: () => Promise<void>) => action() }) }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    })
    const render = () => app.render(platform === 'web' ? 'WorkspaceInputStatus' : 'InputStatus', { pending })
    const button = (label: string) => render().find(({ type, props }) => type === 'Button' && componentText(props.children) === label)?.props
    const press = async (label: string) => { const control = button(label); assert.ok(control); const action = platform === 'web' ? control.onClick : control.onPress; assert.ok(typeof action === 'function'); action(); await settleComponent() }
    try {
      assert.equal(button('重试原输入'), undefined, 'unknown delivery alone never authorizes replay')
      assert.equal(button('关闭提醒'), undefined); assert.equal(button('不再发送'), undefined)
      assert.ok(button('确认原设备并查询'))
      assert.match(componentText(render()), /本人电脑 · original/)
      const namedDevice = shared.info.name
      shared.info.name = ''; assert.match(componentText(render()), /当前连接的设备/)
      shared.info.name = namedDevice
      await press('确认原设备并查询')
      assert.deepEqual(confirmed, [['original', 'original-entry']])
      assert.equal(button('确认原设备并查询'), undefined, 'a confirmed entry cannot be replaced')
      assert.deepEqual(retries, [], 'confirming the original device never invokes retry')
      await press('查询结果')
      const accepted = pending
      assert.ok(accepted.receipt)
      const acceptedReceipt = accepted.receipt
      assert.ok(button('重试原输入'))
      assert.ok(button('关闭提醒'))
      for (const delivery of ['saved', 'unknown', 'uploading', 'submitting'] as const) { pending = { ...accepted, delivery }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined) }
      pending = accepted
      await press('重试原输入')
      assert.deepEqual(retries, ['original'])
      assert.equal(pending, accepted, 'the UI never replaces the original input or its attachments')
      await press('关闭提醒')
      pending = { ...accepted, receipt: { ...acceptedReceipt, retry_safe: false, control_event: null } }
      assert.ok(button('关闭提醒'), 'closing a terminal reminder needs no retry authorization or control event')
      await press('关闭提醒')
      assert.deepEqual(closed, ['original', 'original'])
      for (const stage of ['unknown', 'entry_accepted', 'owner_accepted', 'provider_accepted', 'associated', 'submitting'] as const) { pending = { ...accepted, receipt: { ...acceptedReceipt, stage } }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined) }
      for (const receipt of [{ ...acceptedReceipt, retry_safe: false }, { ...acceptedReceipt, control_event: null }, { ...acceptedReceipt, control_event: '' }, { ...acceptedReceipt, control_event: ' ' }, { ...acceptedReceipt, message_id: 'different' }, { ...acceptedReceipt, target: { kind: 'bot_main' as const, bot_id: 'different' } }]) { pending = { ...accepted, receipt }; assert.equal(button('重试原输入'), undefined) }
      for (const receipt of [{ ...acceptedReceipt, message_id: 'different' }, { ...acceptedReceipt, target: { kind: 'bot_main' as const, bot_id: 'different' } }]) { pending = { ...accepted, receipt }; assert.equal(button('关闭提醒'), undefined) }
      for (const marker of [{ release_expected_event: 'c'.repeat(64) }, { release_command_id: 'a'.repeat(64) }]) {
        pending = { ...accepted, ...marker }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined)
        pending = { ...pending, delivery: 'saved', rejected: '尚未确认' }; assert.equal(button('不再发送'), undefined)
      }
      pending = { ...accepted, rejection_code: 'input_conflict', rejected: '原内容不同' }; assert.equal(button('重试原输入'), undefined); assert.equal(button('关闭提醒'), undefined); assert.equal(button('不再发送'), undefined)
      pending = { ...accepted, delivery: 'unknown' }; assert.match(componentText(render()), /重试请求待确认/); assert.equal(button('重试原输入'), undefined)
      pending = { ...pending, rejected: '尚未确认' }; assert.equal(button('关闭提醒'), undefined); assert.equal(button('不再发送'), undefined)
      pending = accepted
      shared.writable = false; assert.equal(button('重试原输入'), undefined); assert.ok(button('关闭提醒'), 'closing only the local reminder needs no write permission'); shared.writable = true
      const queued: WorkspaceDomainState['inputs'][number] = { input: { report_results: true, message_id: pending.message_id, target, intent: 'defer', sequence: 1, text: pending.text }, conversation_id: 'chat', source_event: 'source', control_event: 'control', effective_order: 1, deferred: true, cancelled: false }
      shared.workspaceState.inputs = [queued]; assert.equal(button('重试原输入'), undefined)
      assert.ok(button('关闭提醒'), 'the matched refused receipt determines closure, not a queue flag')
      shared.workspaceState.inputs = [{ ...queued, deferred: false, cancelled: true }]; assert.equal(button('重试原输入'), undefined); assert.ok(button('关闭提醒'))
      pending = { ...accepted, entry_device_id: undefined, delivery: 'saved', receipt: undefined, rejected: '原输入未保存', rejection_code: 'input_not_saved' }
      shared.workspaceState.inputs = []
      assert.equal(button('重试原输入'), undefined, 'an unbound saved refusal cannot authorize retry')
      assert.match(componentText(render()), /已拒绝 · 原输入未保存/)
      assert.equal(button('确认原设备并查询'), undefined)
      assert.ok(button('不再发送'))
      pending = { ...pending, rejected: undefined }
      assert.match(componentText(render()), /本地已保存/)
      assert.equal(button('重试原输入'), undefined)
      assert.equal(button('确认原设备并查询'), undefined)
      assert.ok(button('不再发送'))
      pending = { ...accepted, entry_device_id: undefined, delivery: 'unknown', receipt: undefined, rejected: '这条输入的送达结果仍未确认，原内容已保留。', rejection_code: 'input_missing' }
      assert.equal(button('重试原输入'), undefined)
      assert.equal(button('查询结果'), undefined)
      assert.equal(button('不再发送'), undefined)
      assert.equal(button('关闭提醒'), undefined)
      assert.match(componentText(render()), /送达结果仍未确认/)
      assert.match(componentText(render()), platform === 'web' ? /此浏览器/ : /此应用/)
      assert.ok(button('确认原设备并查询'))
      shared.connectionOnline = false; assert.equal(button('确认原设备并查询'), undefined); shared.connectionOnline = true
      for (const marker of [{ release_expected_event: 'c'.repeat(64) }, { release_command_id: 'a'.repeat(64) }]) {
        const legacy = pending
        pending = { ...legacy, ...marker }; assert.equal(button('确认原设备并查询'), undefined)
        pending = legacy
      }
      await press('确认原设备并查询')
      assert.deepEqual(confirmed, [['original', 'original-entry'], ['original', 'original-entry']])
      assert.deepEqual(retries, ['original'], 'confirming an unknown missing input stays separate from retry')
      pending = { ...accepted, delivery: 'saved', receipt: undefined, rejected: '原输入未保存', rejection_code: 'input_not_saved' }
      assert.equal(button('关闭提醒'), undefined); assert.ok(button('不再发送'))
      shared.workspaceState.inputs = []
      assert.ok(button('重试原输入'), 'authoritative not-saved evidence exposes the existing separate retry action')
      await press('重试原输入'); assert.deepEqual(retries, ['original', 'original'])
      await press('不再发送'); assert.deepEqual(closed, ['original', 'original', 'original'])
      pending = { ...accepted, delivery: 'saved', receipt: undefined, rejected: undefined, rejection_code: undefined }
      assert.ok(button('不再发送'), 'a later saved input can be discarded before an unknown earlier input is retried')
    } finally { app.close() }
  }
})


test('new task creation uses the displayed default Bot after the catalog loads and the Bot filter changes', async () => {
  for (const mode of ['cold', 'filter']) {
    const other = { ...parityBot, bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' }
    const commands: unknown[] = []
    const shared = { ...parityShared(), bots: [] as ReturnType<typeof parseBot>[], selectedBot: undefined,
      workspaceCommand: async (command: unknown) => { commands.push(command); return { tasks: [{ task_id: 'created-task', revision: 1 }] } },
      sendMessage: async () => true,
    }
    let botId: string | undefined
    if (mode === 'filter') { shared.bots = [parityBot, other]; botId = parityBot.bot_id }
    const app = componentHarness('./workspace-tasks.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/workspace/storage.ts': { newId: () => 'created-task' },
      '@/components/arc/product-kit': new Proxy({ useAction: () => ({ busy: null, error: null, run: async (_key: string, action: () => Promise<void>) => action() }) }, { get: (value, name) => (value as Record<string | symbol, unknown>)[name] ?? String(name) }),
    }, {}, 'CreateWorkspaceTask')
    const render = () => app.render('CreateWorkspaceTask', { open: true, onOpenChange() {}, botId })
    const owner = () => String(render().find(({ type }) => type === 'Select')?.props.value).slice(4)
    try {
      if (mode === 'cold') {
        assert.equal(owner(), '')
        const empty = render().find(({ type }) => type === 'Select')
        assert.equal(empty?.props.value, '', 'required selection stays empty until a Bot is available')
        assert.equal(empty?.props.required, true)
        assert.deepEqual(JSON.parse(JSON.stringify(empty?.props.options)), [])
        shared.bots = [parityBot]
      }
      else { assert.equal(owner(), parityBot.bot_id); botId = other.bot_id; render() }
      const expected = mode === 'cold' ? parityBot.bot_id : other.bot_id
      assert.equal(owner(), expected, 'the visible default is also the command owner without changing the select')
      const fields = render()
      fields.find(({ type }) => type === 'Input')?.props.onChange?.({ currentTarget: { value: '检查界面' } })
      fields.find(({ type }) => type === 'Textarea')?.props.onChange?.({ currentTarget: { value: '检查当前界面的操作' } })
      const submit = render().find(({ type }) => type === 'form')?.props.onSubmit
      assert.ok(submit); submit({ preventDefault() {} }); await settleComponent()
      assert.equal(commands.length, 1)
      assert.equal((commands[0] as { owner_bot: string }).owner_bot, expected)
    } finally { app.close() }
  }
})


test('the required paid transfer budget stays empty until a real budget is selected', () => {
  const budget = { budget_id: 'budget-1', payer: 'wallet', payee: 'payee', provider_peer: 'provider', asset: 'asset', deposit: '10', free: '10', locked: '0', earned: '0', withdrawn: '0', refunded: '0', state: 'open', gas_cap: '1', ready_windows: [] } satisfies DeliveryBudget
  const delivery = { status: { enabled: true, charity_enabled: true, paid_enabled: true, node_peer: 'sender', wallet: { state: 'unlocked', address: 'wallet' }, max_file_bytes: 1024, receiver_key: null }, budgets: [budget], orders: [], providers: [], selected: null, busy: null, canManage: true }
  const app = componentHarness('../lab/transfers.tsx', {
    '@vibeface/client/delivery/client.ts': { useDelivery: () => delivery },
    '@vibeface/client/delivery/model.ts': deliveryModel,
    '@vibeface/client/lib/selection.ts': { selectionOwner },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@/lib/glossary': { terms: new Proxy({}, { get: (_target, name) => ({ label: String(name), icon: String(name) }) }) },
    '@/components/arc/product-kit': { Segmented: 'Segmented', useAction: () => ({ busy: null }) },
  }, {}, 'TransferPage')
  const render = () => app.render('TransferPage', { connection: {} })
  try {
    const mode = render().find(({ type }) => type === 'Segmented')
    assert.ok(mode)
    ;(mode.props.onChange as (value: string) => void)('paid')
    const empty = render().find(({ type, props }) => type === 'Select' && props.id === 'transfer-budget')
    assert.ok(empty)
    assert.equal(empty.props.required, true)
    assert.equal(empty.props.value, '', 'the native required field has no artificial selected value')
    assert.deepEqual(JSON.parse(JSON.stringify(empty.props.options)), [{ value: 'arc:budget-1', label: 'budget-1 · 可分配 10' }])
    ;(empty.props.onValueChange as (value: string) => void)('arc:budget-1')
    assert.equal(render().find(({ type, props }) => type === 'Select' && props.id === 'transfer-budget')?.props.value, 'arc:budget-1')
  } finally { app.close() }
})


test('delivery reconnect fences old action feedback and releases only the new action when it finishes', async () => {
  const peer = '11'.repeat(32)
  const status = { enabled: true, charity_enabled: true, paid_enabled: false, node_peer: peer, wallet: { state: 'absent', address: null }, max_file_bytes: 130816, receiver_key: null }
  const app = componentHarness('../lab/lab.tsx', {
    '@/lib/platform': { webPlatform: { storage: { getItem: () => null, setItem() {} }, onForeground: () => () => {}, foreground: () => false } },
  }, { setInterval: () => 1, clearInterval() {} }, 'ConnectedLab', { '@vibeface/client/delivery/client.ts': '../../../../../packages/client/src/delivery/client.ts' })
  let generation = 0, configureCalls = 0
  const makeConnection = (result: ReturnType<typeof pendingResult>) => {
    const owner = ++generation
    return {
      generation: owner, entry: { device_id: peer }, principal: { source: 'membership', can_manage: true },
      isCurrent: () => generation === owner,
      async request(method: string, params: { op: string }) {
        assert.equal(method, 'delivery')
        switch (params.op) {
          case 'status': return status
          case 'list': return { orders: [] }
          case 'providers': return { providers: [] }
          case 'configure': configureCalls++; return result.promise
          default: assert.fail(`unexpected delivery request: ${params.op}`)
        }
      },
    }
  }
  let connection = makeConnection(pendingResult())
  const render = () => app.render('ConnectedLab', { connection })
  const toggle = () => render().find(({ type }) => type === 'Switch')?.props
  const change = (checked: boolean) => {
    const handler = toggle()?.onCheckedChange
    assert.ok(typeof handler === 'function')
    handler(checked)
  }
  try {
    for (const outcome of ['success', 'failure']) {
      const old = pendingResult(), fresh = pendingResult()
      connection = makeConnection(old); render(); await settleComponent()
      const before = configureCalls
      change(false)
      change(true)
      assert.equal(configureCalls, before + 1)
      assert.equal(toggle()?.disabled, true)
      connection = makeConnection(fresh); render(); await settleComponent()
      assert.equal(toggle()?.disabled, false)
      change(false)
      if (outcome === 'success') old.resolve({ ...status, enabled: false })
      else old.reject(new Error('旧订单结果未知。'))
      await settleComponent()
      assert.equal(toggle()?.disabled, true)
      assert.equal(toggle()?.checked, true)
      assert.equal(render().some(({ props }) => props.error), false)
      fresh.resolve({ ...status, enabled: false }); await settleComponent()
      assert.equal(toggle()?.disabled, false)
      assert.equal(toggle()?.checked, false)
    }
  } finally { app.close() }
})

for (const mobile of [false, true]) for (const settlement of ['current', 'new-file', 'reconnect']) test(`${mobile ? 'Mobile' : 'Web'} transfer prepare settlement honors ${settlement}`, async () => {
  const path = mobile ? '../../../../../apps/mobile/app/transfers.tsx' : '../lab/transfers.tsx'
  const page = mobile ? 'Transfers' : 'TransferPage'
  const files = ['submitted-fixture.txt', 'newer-fixture.txt'].map(name => ({ name, size: 1, dispose() { disposed.push(name) } }))
  const disposed: string[] = []
  let fileIndex = 0, current = true
  const prepared = pendingResult()
  const calls: string[] = []
  const delivery = {
    status: { enabled: true, charity_enabled: true, paid_enabled: false, node_peer: 'sender', wallet: { state: 'absent', address: null }, max_file_bytes: 1024, receiver_key: null },
    orders: [], budgets: [], providers: [], selected: null, busy: null, canManage: true,
    prepare(_draft: unknown, file: { name: string }) { calls.push(file.name); return prepared.promise },
  }
  const picker = () => ({ busy: null, error: '', run: (_key: string, action: () => Promise<void>) => action() })
  const platform = { files: { pick: async () => files[fileIndex++] } }
  const app = componentHarness(path, {
    '@vibeface/client/delivery/client.ts': { useDelivery: () => delivery },
    '@vibeface/client/delivery/model.ts': deliveryModel,
    '@vibeface/client/lib/experiments.ts': { useExperiments: () => [false] },
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@/platform': { mobilePlatform: platform }, '@/lib/platform': { webPlatform: platform },
    '@/lib/use-run': { useRun: picker },
    '@/lib/glossary': { terms: new Proxy({}, { get: (_target, name) => ({ label: String(name), icon: String(name) }) }) },
    '@/components/arc/product-kit': { Segmented: 'Segmented', useAction: picker, Details: 'Details', BusyIcon: 'BusyIcon' },
  }, {}, page)
  const render = () => app.render(page, { connection: { isCurrent: () => current } })
  const choose = () => {
    const handler = render().find(({ type, props }) => type === 'Button' && componentText(props.children).includes('选择文件'))?.props[mobile ? 'onPress' : 'onClick']
    assert.ok(typeof handler === 'function')
    handler()
  }
  try {
    if (mobile) {
      const receiver = render().find(({ type, props }) => typeof type === 'function' && type.name === 'InputField' && props.label === deliveryModel.transferCopy.receiver)
      assert.ok(typeof receiver?.props.onChange === 'function')
      Reflect.apply(receiver.props.onChange, undefined, ['22'.repeat(32)])
    } else {
      const receiver = render().find(({ props }) => props.id === 'transfer-receiver')?.props
      assert.ok(receiver?.onChange)
      receiver.onChange({ target: { value: '22'.repeat(32) } })
    }
    choose(); await settleComponent()
    if (mobile) {
      const submit = render().find(({ type, props }) => type === 'Button' && componentText(props.children).includes(deliveryModel.transferCopy.prepare('charity')))?.props.onPress
      assert.ok(typeof submit === 'function'); submit()
    } else {
      const submit = render().find(({ type }) => type === 'form')?.props.onSubmit
      assert.ok(submit); submit({ preventDefault() {} })
    }
    assert.deepEqual(calls, ['submitted-fixture.txt'])
    if (settlement === 'new-file') { choose(); await settleComponent() }
    prepared.resolve(true)
    if (settlement === 'reconnect') queueMicrotask(() => { current = false })
    await settleComponent()
    assert.deepEqual(disposed, settlement === 'reconnect' ? [] : ['submitted-fixture.txt'])
    const output = componentText(render().map(({ props }) => props.children))
    assert.equal(output.includes('submitted-fixture.txt'), settlement === 'reconnect')
    assert.equal(output.includes('newer-fixture.txt'), settlement === 'new-file')
  } finally { app.close() }
})


test('routine input links preserve their exact trigger', () => {
  const shared = parityShared()
  for (const platform of ['web', 'native']) {
    const app = componentHarness(platform === 'web' ? './workspace-chat.tsx' : '../../../../../apps/mobile/src/features/workspace/workspace.tsx', {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
    })
    try {
      for (const trigger of ['schedule:exact', 'schedule:+/?&中文#%']) {
        const link = app.render(platform === 'web' ? 'InputTargetLink' : 'InputLocation', { conversationId: 'chat', target: { kind: 'routine', bot_id: 'bot', trigger } }).find(({ type }) => type === 'Link')
        assert.ok(link)
        if (platform === 'web') {
          assert.deepEqual(JSON.parse(JSON.stringify(link.props.params)), { botId: 'bot' })
          assert.deepEqual(JSON.parse(JSON.stringify(link.props.search)), { routine: trigger })
        } else {
          assert.equal(typeof link.props.href, 'string')
          const url = new URL(String(link.props.href), 'https://app.vibeface.dev')
          assert.equal(url.pathname, '/b/bot')
          assert.deepEqual([...url.searchParams], [['routine', trigger]])
          assert.equal(url.hash, '')
        }
      }
    } finally { app.close() }
  }
})


test('composers save each pending draft once and let edited drafts save independently', async (t) => {
  const cases = [
    { name: 'web Main', path: '../chat/conversation-screen.tsx', component: 'ConversationScreen', native: false, privateExport: '', props: {} },
    { name: 'web Scoped', path: './workspace-chat.tsx', component: 'ScopedConversation', native: false, privateExport: 'ScopedConversation', props: { target: { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, conversationId: 'chat', channelId: 'channel', label: '项目 Bot' } },
    { name: 'web Work', path: './task-detail.tsx', component: 'TaskDetail', native: false, privateExport: '', props: { botId: 'bot', workId: 'work' } },
    { name: 'native Scoped', path: '../../../../../apps/mobile/src/features/workspace/workspace.tsx', component: 'ScopedWorkspaceScreen', native: true, privateExport: '', props: { channelId: 'channel' } },
    { name: 'native Work', path: '../../../../../apps/mobile/app/b/[botId]/work/[workId].tsx', component: 'TaskDetail', native: true, privateExport: 'TaskDetail', props: { botId: 'bot', workId: 'work' } },
    { name: 'web Scoped Task', path: './workspace-chat.tsx', component: 'ScopedConversation', native: false, privateExport: 'ScopedConversation', props: { target: { kind: 'task', task_id: 'task' }, conversationId: 'chat', label: '项目 Bot' } },
    { name: 'native Scoped Task', path: '../../../../../apps/mobile/src/features/workspace/workspace.tsx', component: 'ScopedWorkspaceScreen', native: true, privateExport: '', props: { taskId: 'task' } },
  ]
  for (const example of cases) await t.test(example.name, async () => {
    type ReadableFile = { name: string; arrayBuffer: () => Promise<ArrayBuffer> }
    const reads = new Map<string, ReturnType<typeof pendingResult>>()
    const read = async (name: string) => { const held = reads.get(name); assert.ok(held); const buffer = await held.promise; assert.ok(buffer instanceof ArrayBuffer); return buffer }
    const attempts: { text: string; files: string[]; target: InputTarget; intent: string | undefined }[] = []
    const saved: { text: string; attachments: { name: string; bytes: number[] }[] }[] = [], saves: Promise<boolean>[] = []
    const navigations: unknown[] = []
    const domain: WorkspaceDomainState = {
      channels: [{ channel_id: 'channel', title: '频道', purpose: '', members: ['bot'], coordinator_bot: 'bot', coordinator_node: 'worker', revision: 1, owner_epoch: 1, event_hash: 'channel-event', created_at_ms: 1 }],
      tasks: [{ task_id: 'task', root_task_id: 'task', owner_bot: 'bot', title: '任务', request: '交代', expected_result: '结果', source_events: [], dependencies: [], reply_only: false, revision: 1, action: 'continue', request_event: 'request', event_hash: 'task-event', created_at_ms: 1, updated_at_ms: 1, state: 'running' }], assignments: [], inputs: [], coverage: { limit: 128, truncated: false },
    }
    const shared = {
      ...parityShared(), data: parseConversation({ ...context, works: [parityWork()] }), workspaceState: domain,
      chatSlot: { conversationId: 'chat', missing: false }, fresh: new Set<string>(), interrupted: [], view: 'chat',
      readWork: async () => ({ work: { ...parityWork(), conversation_id: 'chat', target: null, source_events: [] }, coverage: { source: 'shared_history', complete: false } }),
      readWorkspaceState: async () => domain, readWorkspaceHistory: async (filter: WorkspaceFilter) => ({ history_authority: 'authority', next_cursor: null, complete: true, events: [], coverage: { limit: 128, truncated: false, scope: filter.task_id ? 'task' : 'channel' } }),
      sendMessage: (text: string, files: ReadableFile[], _sent: unknown, options: InputOptions) => {
        attempts.push({ text, files: files.map((file) => file.name), target: options.target, intent: options.intent })
        const taskId = `created-task-${attempts.length}`
        const save = Promise.all(files.map(async (file) => ({ name: file.name, bytes: Array.from(new Uint8Array(await file.arrayBuffer())) }))).then((attachments) => {
          saved.push({ text, attachments })
          options.onSaved?.({ report_results: true, message_id: `input-${taskId}`, conversation_id: 'chat', target: options.intent === 'new_task' ? { kind: 'task', task_id: taskId } : options.target, intent: 'continue', expected_revision: 1, sequence: attempts.length, text, attachments: [], created_at_ms: 1, delivery: 'saved' })
          return true
        })
        saves.push(save)
        return save
      },
    }
    const app = componentHarness(example.path, {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@/data/context': { useWorkspace: () => ({ networkId: 'network', deviceId: 'phone', bots: shared.bots, conversations: {}, members: [], actions: {} }) },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { network_id: 'network', device_id: 'phone', can_manage: false, source: 'membership' }, isCurrent: () => true } }) },
      '@vibeface/client/identity/copy.ts': { setUpOnManager }, '@vibeface/client/lib/node.ts': { providerNames, messageOf }, '@vibeface/client/lib/errors.ts': errors,
      '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
      '@vibeface/client/workspace/bot-settings.ts': { botCopy, folderLabels, placeChanges, scheduleName, scheduleTrigger }, '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage },
      '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters },
      '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', botTint: () => ({}) }, '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) },
      '@/components/kit': { providerIcons: { codex: 'CodexIcon' }, toneText: {} }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
      '@tanstack/react-router': { useNavigate: () => (target: unknown) => { navigations.push(target) } },
      'react-native': { AppState: { currentState: 'active', addEventListener: () => ({ remove() {} }) } },
      'expo-router': { Stack: { Screen: 'Screen' }, useLocalSearchParams: () => ({}), router: { push: (target: unknown) => { navigations.push(target) } }, useRouter: () => ({ push: (target: unknown) => { navigations.push(target) }, back() {} }) },
      'expo-file-system': { File: class { uri: string; constructor(uri: string) { this.uri = uri } arrayBuffer() { return read(this.uri.split('/').at(-1) ?? '') } } },
      'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) },
      '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_key: string, action: () => Promise<void>) => action() }) },
    }, { setInterval: () => 1, clearInterval() {}, window: { addEventListener() {}, removeEventListener() {}, matchMedia: () => ({ matches: true }) } }, example.privateExport)
    const render = () => app.render(example.component, example.props)
    const composer = () => { const props = render().find(({ type }) => type === (example.native ? 'Composer' : 'PromptInput'))?.props; assert.ok(props); return props }
    const submit = () => { const send = composer()[example.native ? 'onSend' : 'onSubmit']; assert.ok(typeof send === 'function'); send() }
    const attach = (name: string) => {
      if (example.native) { const remove = composer().onFiles; assert.ok(typeof remove === 'function'); remove([]) }
      const input = composer()[example.native ? 'onPickFiles' : 'onFiles']; assert.ok(typeof input === 'function')
      const files = example.native ? [{ uri: `file:///${name}`, name, size: 3, mimeType: 'text/plain' }] : [{ name, arrayBuffer: () => read(name) }]
      input(example.native ? files.map(file => ({ ...file, arrayBuffer: () => read(name), text: async () => '', dispose() {} })) : files)
      return example.native ? composer().files : files
    }
    try {
      render(); await settleComponent()
      if (example.component === 'ScopedWorkspaceScreen') {
        const target: InputTarget = example.name.endsWith('Task') ? { kind: 'task', task_id: 'task' } : { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }
        const first = pendingResult(); reads.set('first.txt', first)
        composer().onChange?.('第一份草稿'); attach('first.txt'); await settleComponent()
        const original = app.drafts.read({ networkId: 'network', deviceId: 'phone', conversationId: 'chat', inputTarget: inputTargetKey(target) })
        submit(); submit(); await settleComponent()
        assert.equal(attempts.length, 0, 'native preparation reads the complete attachment before local enqueue')
        assert.equal(composer().sending, true)
        const later = pendingResult(); reads.set('later.txt', later)
        composer().onChange?.('第二份草稿'); attach('later.txt'); await settleComponent(); submit(); await settleComponent()
        assert.equal(attempts.length, 0, 'one native scope keeps a single preparation while its newer draft remains editable')
        assert.ok(app.ownsDraftFile(original.files[0].uri), 'the submitted attachment lease survives replacement by the new draft')
        first.resolve(new Uint8Array([1, 2, 3]).buffer); await settleComponent(); await saves[0]; await settleComponent()
        assert.equal(attempts.length, 1)
        assert.equal(composer().value, '第二份草稿')
        assert.deepEqual(JSON.parse(JSON.stringify(composer().files)), [{ uri: 'file:///later.txt', name: 'later.txt', size: 3, mimeType: 'text/plain' }], 'acceptance of the first draft preserves the newer attachments')
        assert.equal(app.ownsDraftFile(original.files[0].uri), false, 'the retired submitted copy releases after byte preparation and acceptance')
        submit(); submit(); await settleComponent()
        assert.equal(attempts.length, 1, 'the next native draft still waits for its own attachment bytes')
        later.resolve(new Uint8Array([4, 5, 6]).buffer); await settleComponent(); await saves[1]; await settleComponent()
        assert.equal(attempts.length, 2)
        assert.equal(composer().value, '')
        assert.deepEqual(JSON.parse(JSON.stringify(composer().files)), [])
        assert.deepEqual(saved, [
          { text: '第一份草稿', attachments: [{ name: 'first.txt', bytes: [1, 2, 3] }] },
          { text: '第二份草稿', attachments: [{ name: 'later.txt', bytes: [4, 5, 6] }] },
        ])
        assert.deepEqual(JSON.parse(JSON.stringify(attempts.map(item => item.target))), [target, target])
        const failed = pendingResult(); reads.set('retry.txt', failed)
        composer().onChange?.('原样重试'); attach('retry.txt'); await settleComponent(); submit(); await settleComponent()
        failed.reject(new Error('file unavailable')); await settleComponent()
        assert.equal(attempts.length, 2, 'a failed native read cannot enqueue an incomplete input')
        assert.equal(composer().value, '原样重试')
        const retryFiles = composer().files
        assert.ok(Array.isArray(retryFiles)); assert.equal(workspaceModel.record(retryFiles[0]).name, 'retry.txt')
        assert.equal(render().find(({ type }) => type === 'ErrorText')?.props.error, messageOf(new Error('file unavailable')))
        const retry = pendingResult(); reads.set('retry.txt', retry); submit(); submit(); await settleComponent()
        retry.resolve(new Uint8Array([7, 8, 9]).buffer); await settleComponent(); await saves[2]; await settleComponent()
        assert.equal(attempts.length, 3, 'a failed preparation releases the scope for exactly one retry')
        assert.equal(composer().value, '')
        assert.deepEqual(saved.at(-1), { text: '原样重试', attachments: [{ name: 'retry.txt', bytes: [7, 8, 9] }] })
        return
      }
      const first = pendingResult(); reads.set('first.txt', first)
      composer().onChange?.('第一份草稿'); attach('first.txt'); submit(); submit()
      composer().onChange?.('第一份草稿'); submit(); await settleComponent()
      assert.equal(attempts.length, 1, 'unchanged redraws, taps and text events keep the pending draft identity')
      assert.equal(saved.length, 0, 'the first attachment is still preparing before local acceptance')
      const later = pendingResult(); reads.set('later.txt', later)
      composer().onChange?.('第二份草稿'); const laterFiles = attach('later.txt'); submit(); await settleComponent()
      assert.equal(attempts.length, 2, 'editing can submit a new draft while the first attachment is blocked')
      first.resolve(new Uint8Array([1, 2, 3]).buffer); await saves[0]; await settleComponent()
      assert.equal(composer().value, '第二份草稿')
      assert.equal(composer().files, laterFiles, 'the earlier save cannot remove the new attachments')
      later.resolve(new Uint8Array([4, 5, 6]).buffer); await Promise.all(saves); await settleComponent()
      assert.equal(composer().value, '')
      assert.deepEqual(JSON.parse(JSON.stringify(composer().files)), [])
      assert.deepEqual(JSON.parse(JSON.stringify(attempts.map(({ text, files, intent }) => ({ text, files, intent })))), [
        { text: '第一份草稿', files: ['first.txt'], intent: 'continue' }, { text: '第二份草稿', files: ['later.txt'], intent: 'continue' },
      ], 'the composer sends the prompt intent')
      assert.deepEqual(saved, [
        { text: '第一份草稿', attachments: [{ name: 'first.txt', bytes: [1, 2, 3] }] }, { text: '第二份草稿', attachments: [{ name: 'later.txt', bytes: [4, 5, 6] }] },
      ])
      const target: InputTarget = example.name.endsWith('Task') ? { kind: 'task', task_id: 'task' } : example.name === 'web Main' ? { kind: 'bot_main', bot_id: 'bot' } : example.component.includes('Scoped') ? { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' } : { kind: 'work', work_id: 'work' }
      if (example.native) assert.equal(composer().attachmentScope, JSON.stringify(['network', 'chat', target.kind === 'work' ? ['work', target.work_id] : inputTargetKey(target)]))
      assert.deepEqual(JSON.parse(JSON.stringify(attempts.map((item) => item.target))), [target, target])
      const failed = pendingResult(); reads.set('retry.txt', failed)
      composer().onChange?.('原样重试'); const retryFiles = attach('retry.txt'); submit()
      failed.reject(new Error('file unavailable')); await assert.rejects(saves[2], /file unavailable/); await settleComponent()
      assert.equal(composer().value, '原样重试'); assert.equal(composer().files, retryFiles)
      const retry = pendingResult(); reads.set('retry.txt', retry); submit(); submit(); await settleComponent()
      assert.equal(attempts.length, 4, 'a failed save releases the draft guard for exactly one retry')
      retry.resolve(new Uint8Array([7, 8, 9]).buffer); await saves[3]; await settleComponent()
      assert.equal(composer().value, '')
      assert.deepEqual(saved.at(-1), { text: '原样重试', attachments: [{ name: 'retry.txt', bytes: [7, 8, 9] }] })
      if (!example.name.endsWith('Scoped')) for (const change of ['text', 'unchanged']) {
        const before = navigations.length, taskId: string = `created-task-${attempts.length + 1}`, name = `navigate-${change}.txt`
        const held = pendingResult(); reads.set(name, held)
        composer().onChange?.(`新任务 ${change}`); attach(name); submit()
        if (change === 'text') composer().onChange?.('当前正在编辑的正文')
        render()
        held.resolve(new Uint8Array([10, 11, 12]).buffer); await saves.at(-1); await settleComponent()
        assert.equal(navigations.length, before + (change === 'unchanged' ? 1 : 0), 'only the unchanged submitted draft can navigate after local acceptance')
        if (change === 'unchanged') {
          assert.deepEqual(JSON.parse(JSON.stringify(navigations.at(-1))), example.native ? `/task/${taskId}` : { to: '/tasks/$taskId', params: { taskId } })
          assert.equal(composer().value, '')
        } else assert.equal(composer().value, '当前正在编辑的正文', 'late navigation cannot discard the active draft')
      }
    } finally {
      for (const held of reads.values()) held.resolve(new ArrayBuffer(0))
      await Promise.allSettled(saves)
      app.close()
    }
  })
})


test('both channel composers retain the durable task target before the domain snapshot arrives', async () => {
  for (const platform of ['web', 'native']) {
    const domain: WorkspaceDomainState = {
      channels: [{ channel_id: 'channel', title: '官网改版', purpose: '', members: ['bot', 'other'], coordinator_bot: 'bot', coordinator_node: 'worker', revision: 1, owner_epoch: 1, event_hash: 'channel-event', created_at_ms: 1 }],
      tasks: [], assignments: [], inputs: [], coverage: { limit: 128, truncated: false },
    }
    const durable: PendingMessage = { report_results: true, message_id: 'locally-saved', conversation_id: 'chat', target: { kind: 'channel_task', channel_id: 'channel', task_id: 'stable-task', bot_id: 'bot' }, intent: 'continue', expected_revision: 1, sequence: 1, text: 'first', created_at_ms: 1, delivery: 'saved' }
    const pendingMessages: PendingMessage[] = []
    const sends: InputOptions[] = []
    let nativeSaveHold: ReturnType<typeof pendingResult> | null = null
    const shared = { ...parityShared(), workspaceState: domain, pendingMessages,
      readWorkspaceState: async () => domain,
      readWorkspaceHistory: async (filter: WorkspaceFilter) => ({ history_authority: 'authority', next_cursor: null, complete: true, events: [], coverage: { limit: 128, truncated: false, scope: filter.task_id ? 'task' : 'channel' } }),
      sendMessage: async (_text: string, _files: unknown[], _sent: unknown, options?: InputOptions) => { assert.ok(options); sends.push(options); await nativeSaveHold?.promise; const saved: PendingMessage = options.target.kind === 'channel_member' && options.target.bot_id === 'other' ? { ...durable, conversation_id: 'other-chat', target: { kind: 'channel_task', channel_id: 'channel', task_id: 'other-task', bot_id: 'other' } } : durable; shared.pendingMessages = [saved]; options.onSaved?.(saved); return true },
    }
    shared.bots.push({ ...parityBot, bot_id: 'other', conversation_id: 'other-chat', name: '另一个 Bot' })
    // A cached recipient without its task is ignored: the Channel opens on its coordinator.
    const saved = new Map<string, string>([[`vibeface.${networkKey('channel-target:channel', 'network')}`, JSON.stringify({ bot_id: 'other' })]])
    const storage = { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value) } }
    const dependencies = {
      '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared },
      '@/data/context': { useWorkspace: () => ({ networkId: 'network', deviceId: 'phone', bots: shared.bots, conversations: {}, members: [], actions: {} }) },
      '@vibeface/client/chat/protocol.ts': { promptIntent, inputTargetKey },
      '@vibeface/client/identity/context.tsx': { useDevice: () => ({ ready: { principal: { network_id: 'network', device_id: 'phone' }, isCurrent: () => true } }) },
      '@/lib/platform': { webPlatform: { storage } }, '@/platform': { mobilePlatform: { storage } },
      'react-native': { AppState: { currentState: 'active', addEventListener: () => ({ remove() {} }) } },
      'expo-router': { Stack: { Screen: 'Screen' }, router: { push() {} }, useLocalSearchParams: () => ({}) }, 'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 0 }) },
      '@/lib/use-run': { useRun: () => ({ busy: null, error: '', run: async (_key: string, action: () => Promise<void>) => action() }) },
      '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
    }
    const source = platform === 'web' ? './workspace-chat.tsx' : '../../../../../apps/mobile/src/features/workspace/workspace.tsx'
    const make = () => componentHarness(source, dependencies, { setInterval: () => 1, clearInterval() {} })
    let app = make()
    try {
      if (platform === 'web') {
        const scoped = () => app.render('ChannelWorkspace', { channelId: 'channel' }).find(({ type }) => typeof type === 'function' && type.name === 'ScopedConversation')?.props
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' })
        const accepted = scoped()?.onSaved
        assert.ok(typeof accepted === 'function')
        shared.pendingMessages = [durable]
        accepted(durable)
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), durable.target)
        assert.equal(scoped()?.expectedRevision, 1)
        assert.equal(scoped()?.task, undefined, 'the local target does not invent a server task projection')
        app.close(); app = make()
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), durable.target, 'a reload restores the selected task from durable input evidence')
        assert.equal(scoped()?.expectedRevision, 1)
        const previousSaved = scoped()?.onSaved
        const endReply = app.render('ChannelWorkspace', { channelId: 'channel' }).find(({ type, props }) => type === 'Button' && componentText(props.children) === '结束回复选择')?.props
        assert.ok(typeof endReply?.onClick === 'function'); endReply.onClick()
        scoped()
        assert.ok(typeof previousSaved === 'function'); previousSaved(durable)
        assert.deepEqual(JSON.parse(JSON.stringify(scoped()?.target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, 'a late save cannot undo a later end of the reply selection')
      } else {
        const composer = () => app.render('ScopedWorkspaceScreen', { channelId: 'channel' }).find(({ type }) => type === 'Composer')?.props
        composer()?.onChange?.('first')
        const first = composer()?.onSend
        assert.ok(typeof first === 'function'); first(); await settleComponent()
        composer()?.onChange?.('second')
        const second = composer()?.onSend
        assert.ok(typeof second === 'function'); second(); await settleComponent()
        assert.deepEqual(sends.map((input) => JSON.parse(JSON.stringify(input.target))), [{ kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, durable.target])
        assert.equal(sends[1].expected_revision, 1)
        app.close(); app = make()
        composer()?.onChange?.('after reload')
        const afterReload = composer()?.onSend
        assert.ok(typeof afterReload === 'function'); afterReload(); await settleComponent()
        assert.deepEqual(JSON.parse(JSON.stringify(sends[2].target)), durable.target)
        assert.equal(sends[2].expected_revision, 1)
        const endReply = app.render('ScopedWorkspaceScreen', { channelId: 'channel' }).find(({ props }) => props.accessibilityLabel === '结束回复这个任务')?.props
        assert.ok(typeof endReply?.onPress === 'function'); endReply.onPress()
        composer()
        domain.tasks.push({ task_id: 'stable-task', root_task_id: 'stable-task', channel_id: 'channel', owner_bot: 'bot', title: '稍后同步的任务', request: '交代', expected_result: '结果', source_events: [], dependencies: [], reply_only: false, revision: 1, action: 'continue', request_event: 'request', event_hash: 'task-event', created_at_ms: 1, updated_at_ms: 1, state: 'running' })
        sends[2].onSaved?.(durable)
        composer()?.onChange?.('交给协调 Bot')
        const sendCoordinator = composer()?.onSend
        assert.ok(typeof sendCoordinator === 'function'); sendCoordinator(); await settleComponent()
        assert.deepEqual(JSON.parse(JSON.stringify(sends[3].target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' }, 'late metadata and an old save cannot redirect the next native input back to an explicitly ended task')
        const endAgain = app.render('ScopedWorkspaceScreen', { channelId: 'channel' }).find(({ props }) => props.accessibilityLabel === '结束回复这个任务')?.props.onPress
        assert.ok(typeof endAgain === 'function'); endAgain()
        nativeSaveHold = pendingResult()
        composer()?.onChange?.('还在保存的旧交代')
        const oldSave = composer()?.onSend
        assert.ok(typeof oldSave === 'function'); oldSave(); await settleComponent()
        composer()?.onChange?.('当前正在编辑的新交代'); await settleComponent()
        nativeSaveHold.resolve(undefined); nativeSaveHold = null; await settleComponent()
        assert.equal(composer()?.value, '当前正在编辑的新交代', 'an accepted older draft keeps the current channel editor')
        assert.equal(composer()?.attachmentScope, JSON.stringify(['network', 'phone', 'chat', inputTargetKey({ kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' })]), 'an older save cannot auto-select its new task over the newer channel draft')
        const newSave = composer()?.onSend
        assert.ok(typeof newSave === 'function'); newSave(); await settleComponent()
        assert.deepEqual(JSON.parse(JSON.stringify(sends.at(-1)?.target)), { kind: 'channel_member', channel_id: 'channel', bot_id: 'bot' })
      }
    } finally { app.close() }
  }
})


test('empty chat starters preserve an editable unsent draft', () => {
  let sends = 0
  const shared = { ...parityShared(), fresh: new Set<string>(), interrupted: [], chatSlot: { conversationId: 'chat', missing: false }, sendMessage: async () => { sends++; return true } }
  const harness = conversationHarness(() => shared)
  const suggestions = () => harness.render().filter(({ type, props }) => (type === 'Button' || type === 'button') && starters.general.includes(componentText(props.children)))
  try {
    assert.equal(harness.chatEntries().length, 0)
    assert.equal(suggestions().length, starters.general.length)
    const starter = suggestions()[0]
    assert.ok(starter?.props.onClick)
    const example = componentText(starter.props.children)
    starter.props.onClick()
    assert.equal(harness.composer()?.value, example)
    assert.equal(suggestions().length, 0)
    assert.doesNotMatch(componentText(harness.render()), /选一句填入草稿/)
    assert.equal(sends, 0)

    const change = harness.composer()?.onChange
    assert.ok(typeof change === 'function')
    const edited = `${example}，保留我补充的要求。`
    change(edited)
    assert.equal(harness.composer()?.value, edited)
    assert.equal(suggestions().length, 0)
    change('')
    assert.equal(harness.composer()?.value, '')
    assert.equal(suggestions().length, starters.general.length)
    change(' \n\t')
    assert.equal(harness.composer()?.value, ' \n\t')
    assert.equal(suggestions().length, starters.general.length)
    assert.equal(sends, 0)
  } finally { harness.app.close() }
})


test('chat task groups stay at their originating message and orphan tasks remain available only in history', () => {
  const messages = [
    { message_id: 'one', event_hash: 'one', role: 'user', text: '核对项目', created_at_ms: 1 },
    { message_id: 'reply', event_hash: 'reply', role: 'assistant', text: '已开始核对', created_at_ms: 2, in_reply_to: 'one' },
    { message_id: 'two', event_hash: 'two', role: 'user', text: '继续', created_at_ms: 3 },
  ]
  const complete = (work_id: string, trigger = 'message:one') => parityWork({ work_id, trigger, state: 'finished', outcome: 'completed', title: `已完成的 ${work_id}`, result_text: '不在聊天重复的结果' })
  const shared = { ...parityShared(), fresh: new Set<string>(), interrupted: [], chatSlot: { conversationId: 'chat', missing: false } }
  shared.data = parseConversation({ ...context, messages, works: [complete('root'), complete('sibling'), parityWork({ work_id: 'child', trigger: 'task:root' }), complete('orphan', 'message:missing')] })
  const harness = conversationHarness(() => shared)
  try {
    const entries = harness.chatEntries()
    assert.deepEqual(entries.map((entry) => entry.key), ['one', 'task-group:message:one', 'task-group:task:root', 'reply', 'two'])
    const group = entries.find((entry) => entry.key === 'task-group:message:one')?.node
    assert.equal(group?.type, 'Details')
    assert.equal(group?.props.label, '2 个任务 · 已完成')
    const elements = harness.render()
    const cards = elements.filter(({ type }) => typeof type === 'function' && type.name === 'TaskCard')
    assert.equal(cards.length, 3, 'the orphan cannot create a chat row')
    const finished = cards.find(({ props }) => (props.task as { id: string }).id === 'sibling')
    assert.ok(finished)
    assert.equal(componentText(nestedElements([finished], ['TaskCard']).find(({ type }) => type === 'div')), '已完成 · 查看任务')
    const history = elements.find(({ type }) => typeof type === 'function' && type.name === 'TaskHistory')
    assert.equal(history, undefined, 'an inactive chat never mounts a history portal')
    const active = harness.render({ active: true })
    const historyKey = active.find(({ type }) => typeof type === 'function' && type.name === 'TaskHistory')?.key
    const chatKey = active.find(({ type }) => type === 'Chat')?.key
    assert.ok(historyKey && chatKey)
    assert.notEqual(historyKey, chatKey, 'history and the retained chat are different siblings during reconciliation')
    shared.data = parseConversation({ ...context, messages: messages.slice(0, 1), works: shared.data.works })
    assert.equal(harness.chatEntries()[1].key, 'task-group:message:one', 'a reply arriving never moves the task group')
    shared.data = parseConversation({ ...context, messages, works: [complete('root'), complete('sibling'), complete('child', 'task:root'), complete('orphan', 'message:missing')] })
    assert.equal(harness.chatEntries()[2].key, 'task-group:task:root', 'progress changes retain the group key')
  } finally { harness.app.close() }
})


test('chat task history keeps completed and unknown records while counting only current attention', () => {
  const approval = { question_id: 'approval', conversation_id: 'chat', target: { kind: 'bot_main', bot_id: 'bot' }, attention: { kind: 'approval', device_id: 'worker' }, severity: 'attention', text: '允许继续吗？', choices: [{ label: '继续' }], author: 'worker', created_at_ms: 2, answer: null, closed: null, about: { node: 'worker', agent: 'codex', session_id: 'session', cwd: '/project', summary: '', request_id: 'request' } }
  const failed = parityWork({ work_id: 'failed', state: 'finished', outcome: 'failed', failure_reason: '已保留修改' })
  const openWorks: (WorkRecord & { conversation_id: string })[] = []
  const shared = { ...parityShared(), openWorks, fresh: new Set<string>(), interrupted: [], chatSlot: { conversationId: 'chat', missing: false } }
  shared.data = parseConversation({ ...context, works: [parityWork(), failed, parityWork({ work_id: 'unknown', state: 'unverified', outcome: 'unverified', failure_reason: '结果无法确认' }), parityWork({ work_id: 'completed', state: 'finished', outcome: 'completed' })], questions: [approval] })
  shared.openWorks = shared.data.works.map((work) => ({ ...work, conversation_id: 'chat' }))
  const dependencies = {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels, '@/lib/glossary': { terms: { approval: { icon: 'ApprovalIcon' } } },
  }
  const app = componentHarness('../chat/conversation-screen.tsx', dependencies, {}, 'TaskHistory, TaskIcon')
  const harness = conversationHarness(() => shared)
  const render = () => {
    const history = harness.render({ active: true }).find(({ type }) => typeof type === 'function' && type.name === 'TaskHistory')
    assert.ok(history)
    return app.render('TaskHistory', history.props)
  }
  const label = () => render().find(({ type }) => type === 'Button')?.props['aria-label']
  try {
    assert.equal(label(), '执行记录 · 3 待处理')
    assert.equal(render().filter(({ type, props }) => type === 'Link' && props.to === '/b/$botId/work/$workId').length, 4, 'history includes completed, missing-origin and uncertain execution records')
    assert.equal(render().filter(({ type, props }) => type === 'Link' && props.to === '/tasks').length, 1, 'task conversations have a separate navigation entry')
    assert.match(componentText(render()), /已完成/)
    assert.match(componentText(render()), /待确认/)
    assert.equal(app.render('TaskIcon', { state: 'unknown', stale: false }).some(({ type }) => type === 'CircleQuestionMark'), true)
    assert.equal(app.render('TaskIcon', { state: 'completed', stale: false }).some(({ type }) => type === 'CircleCheck'), true)
    shared.answered = { approval: '继续' }
    shared.openWorks = shared.openWorks.filter((work) => work.work_id !== 'failed')
    assert.equal(label(), '执行记录 · 1 进行中 · 1 待处理', 'answered approvals and closed historical failures stop counting')
    const trigger = render().find(({ type }) => type === 'Button')
    let restoredFocus = 0
    const triggerRef = trigger?.props.ref as { current: { focus(options?: { preventScroll?: boolean }): void } | null }
    triggerRef.current = { focus(options) { assert.equal(options?.preventScroll, true); restoredFocus++ } }
    trigger?.props.onClick?.()
    assert.equal(render().find(({ type }) => type === 'Drawer')?.props.open, true)
    const onCloseAutoFocus = render().find(({ type }) => type === 'DrawerContent')?.props.onCloseAutoFocus as (event: { preventDefault(): void }) => void
    render().find(({ type }) => type === 'Link')?.props.onClick?.()
    assert.equal(render().find(({ type }) => type === 'Drawer')?.props.open, false, 'navigation closes the sheet first')
    let prevented = false
    const closeFocus = () => onCloseAutoFocus({ preventDefault() { prevented = true } })
    closeFocus()
    assert.equal(prevented, true, 'detail navigation owns focus after the sheet closes')
    assert.equal(restoredFocus, 0, 'detail navigation does not focus a stale chat action')
    render().find(({ type }) => type === 'Button')?.props.onClick?.()
    prevented = false
    closeFocus()
    assert.equal(prevented, true, 'ordinary dismissal explicitly handles the portalled opener')
    assert.equal(restoredFocus, 1, 'ordinary dismissal restores the actual opening button')
    shared.connectionOnline = false
    assert.equal(label(), '执行记录 · 2 待处理', 'offline active tasks are uncertain instead of counted as running')
  } finally { app.close(); harness.app.close() }
})
