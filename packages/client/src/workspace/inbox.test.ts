import assert from 'node:assert/strict'
import test from 'node:test'
import { availability, parseNodeSettings, summaryReadiness, type JoinRequest, type ProviderStatus } from '../lib/node.ts'
import { inboxItems, waitingItems, type InboxSource } from './inbox.ts'
import { cardState, chatTaskState, deviceForm, deviceOnline, entryLabel, externalSessionLabels, externalSessionStatus, fixOnDevice, formName, parseSummarySteps, promptSummary, schedulerStatus, staleTaskLabel, taskGroup, taskLabel, taskStale } from './labels.ts'
import type { BotRecord, NodeInfo, NotificationRecord, Question, WorkCard, WorkRecord } from './types.ts'

const question = (id: string, closed = false): Question => ({
  question_id: id, conversation_id: 'c', bot: { bot_id: 'b', name: '助手' }, severity: 'normal', text: id, choices: [], about: { kind: 'general' },
  author: 'node', created_at_ms: 1, answer: null, closed: closed ? { reason: 'expired', at_ms: 2 } : null,
} as unknown as Question)
const join = (status: JoinRequest['status']): JoinRequest => ({
  join_id: status, status, network_id: 'n', invite_id: 'i', device_id: 'phone', kind: 'client', name: '我的手机', commit: 'c',
  admin_id: null, admin_nonce: null, joiner_nonce: null, reason: null, expires_at_ms: 1,
})
const unready: NodeInfo = { device_id: 'mac', name: 'Mac', local: true, online: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'needs_setup', relay: false, roots: [] } }
const botRecord: BotRecord = { bot_id: 'b', conversation_id: 'c', name: '助手', instructions: '', models: {}, skills: null, avatar: null, avatar_version: null, event_hash: 'h', last_message: null, busy: null }

const source = (manage: boolean, answered: Record<string, string> = {}): InboxSource => ({
  device: { ready: { principal: { network_id: 'n', device_id: 'me', epoch: 1, source: 'membership', can_manage: manage }, entry: { device_id: 'mac' } }, joins: [join('pending'), join('joined')], pendingRecovery: null, rotateHint: false },
  workspace: {
    writeState: { state: 'writable', conflicts: [] }, questions: [question('open'), question('closed', true)], answered, openWorks: [], nodes: [unready], data: null,
    messagePending: null, messageBusy: false, selectedBot: null, notifications: [], ignoredNotifications: new Set(), bots: [],
  },
})

test('待处理 counts the same items wherever it shows: open questions, joins to approve and a bot device to set up', () => {
  const items = inboxItems(source(true))
  assert.deepEqual(items.map((item) => item.kind), ['question', 'join', 'setup'])
  assert.equal(waitingItems(items).length, 3)
  assert.equal(waitingItems(inboxItems(source(true, { open: 'ok' }))).length, 2, 'a question this device just answered no longer counts')
  assert.deepEqual(inboxItems(source(false)).map((item) => item.kind), ['question'], 'only a manager is asked to approve or set up')
  const starting = source(true)
  starting.workspace.nodes = [{ ...unready, capabilities: { ...unready.capabilities!, manager_state: 'starting' } }]
  assert.deepEqual(inboxItems(starting).map((item) => item.kind), ['question', 'join'], 'a bot still starting is not a device to set up')
  starting.workspace.nodes.push({ ...unready, device_id: 'pc', local: false })
  assert.deepEqual(inboxItems(starting).find((item) => item.kind === 'setup'), { key: 'setup', kind: 'setup', device: 'pc', title: '运行 Bot 的设备需设置' }, 'one device starting does not hide another that needs setup')
  const unverified = source(true)
  unverified.workspace.nodes = [{ ...unready, capabilities: { ...unready.capabilities!, manager_state: 'unverified' } }]
  assert.equal(inboxItems(unverified).some((item) => item.kind === 'setup'), false, 'unverified login does not require setup')
})

test('a different pending recovery gets a fresh confirmation card', () => {
  const current = source(true)
  const recovery = { checkpoint: 'first', effective_at_ms: 1, members: [] }
  current.device.pendingRecovery = recovery
  const first = inboxItems(current).find((item) => item.kind === 'recovery')
  assert.ok(first)
  current.device.pendingRecovery = { ...recovery, effective_at_ms: 2 }
  assert.equal(inboxItems(current).find((item) => item.kind === 'recovery')?.key, first.key)
  current.device.pendingRecovery = { ...recovery, checkpoint: 'replacement' }
  const replacement = inboxItems(current).find((item) => item.kind === 'recovery')
  assert.ok(replacement)
  assert.notEqual(replacement.key, first.key)
  current.device.pendingRecovery = null
  assert.equal(inboxItems(current).some((item) => item.kind === 'recovery'), false)
})

test('a failed task waits in 待处理 under its bot until something follows it up, naming its device', () => {
  const failed = source(false)
  const work = { work_id: 'w1', conversation_id: 'c', trigger: '', target_node: 'mac', cwd: '/w', title: '跑测试', prompt: '', state: 'finished', summary: '', outcome: 'failed', result_text: '', failure_reason: 'timed out' } as const
  failed.workspace.openWorks = [{ ...work, resolved: { provider: 'codex', model: null } }, { ...work, work_id: 'w2', outcome: null, state: 'running', resolved: { provider: 'codex', model: null } }, { ...work, work_id: 'w3', title: '', outcome: null, state: 'dispatch_rejected' }]
  failed.workspace.bots = [botRecord]
  assert.deepEqual(inboxItems(failed).filter((item) => item.kind === 'failed').map((item) => [item.key, item.title, item.meta]), [['failed:w1', '跑测试', 'Mac'], ['failed:w3', '任务没有完成', 'Mac']], 'a refused dispatch waits like a failure')
})

test('a session handoff reminder points at its Bot and keeps its text', () => {
  const handoff = source(true)
  handoff.workspace.bots = [botRecord]
  handoff.workspace.notifications = [{ notification_id: 'n-session', kind: 'session_attributed', text: 'tests fail', conversation_id: 'c' }]
  const item = inboxItems(handoff).find((entry) => entry.kind === 'session')
  assert.equal(item?.kind, 'session')
  if (item?.kind === 'session') assert.deepEqual([item.bot.bot_id, item.notification.text], ['b', 'tests fail'])
})

test('a phone asking to join reads as a phone, and only a computer has an online state', () => {
  assert.equal(formName[deviceForm('client', join('pending').name)], '手机')
  assert.equal(formName[deviceForm('client', 'Chrome')], '浏览器')
  assert.equal(formName[deviceForm('client', '我的 iPad')], '平板')
  assert.equal(formName[deviceForm('node', '我的手机')], '电脑')
  assert.equal(deviceOnline('client', undefined, false, true), undefined, 'no device knows whether a browser on another connection is open')
  assert.equal(deviceOnline('client', undefined, false, true, true), true, 'the browser in hand is online while connected')
  assert.equal(deviceOnline('client', undefined, false, false, true), false)
  assert.equal(deviceOnline('node', { online: false }, true, true), false)
  assert.equal(deviceOnline('node', undefined, true, true), true, 'the connected computer is online before the device list arrives')
  assert.equal(deviceOnline('node', { online: true }, true, false), false, 'the computer this device lost reads offline, not its last report')
  assert.equal(deviceOnline('node', { online: true }, false, false), undefined, 'another computer is unknown while this device is offline')
})

test('a task done itself waits on a child still running, down the chain, and only then', () => {
  const work = (work_id: string, extra: Partial<WorkCard> = {}): WorkCard => ({ work_id, target_node: 'mac', cwd: '/w', title: work_id, state: 'finished', summary: '', outcome: 'completed', failure_reason: null, ...extra })
  const parent = work('p')
  assert.equal(chatTaskState(parent, [parent]), 'completed')
  assert.equal(chatTaskState(parent, [parent, work('c', { parent: 'p', state: 'running', outcome: null })]), 'awaiting_children')
  assert.equal(taskLabel('awaiting_children'), '等子任务')
  assert.equal(chatTaskState(parent, [parent, work('c', { parent: 'p' }), work('g', { parent: 'c', state: 'accepted', outcome: null })]), 'awaiting_children', 'a grandchild still running holds it too')
  assert.equal(chatTaskState(parent, [parent, work('c', { parent: 'p' })]), 'completed', 'finished children do not')
  assert.equal(chatTaskState(work('f', { outcome: 'failed' }), [work('c', { parent: 'f', state: 'running', outcome: null })]), 'failed', 'a failure still reads as one')
})

test('the device to fix a refused task is chosen by its kind, not its words', () => {
  assert.equal(fixOnDevice({ failure: 'no_bot' }), true)
  assert.equal(fixOnDevice({ failure: 'quota' }), false)
  assert.equal(fixOnDevice({}), false)
})

test('the capsule names the computer from the directory once it lists it, not by the name it had before joining', () => {
  const directory = { members: [{ device_id: 'b', kind: 'node' as const, name: '工作电脑', manage: false }] }
  assert.equal(entryLabel({ joined: true, directory, entryId: 'b', ownName: '我的 Mac', address: 'http://127.0.0.1:1' }), '工作电脑')
  assert.equal(entryLabel({ joined: true, directory: null, entryId: 'b', ownName: '我的 Mac', address: 'http://127.0.0.1:1' }), null)
  assert.equal(entryLabel({ joined: false, directory: null, entryId: 'b', ownName: '我的 Mac', address: 'http://127.0.0.1:1' }), '我的 Mac')
})

test('an installed provider remains unverified until the device confirms readiness', () => {
  type Status = Pick<ProviderStatus, 'installed' | 'roles' | 'readiness' | 'execution_readiness'>
  const unverified: ProviderStatus['readiness'] = { state: 'unverified', issue: { stage: 'model_list', code: 'model_unverified' } }
  const provider = (extra: Partial<Status> = {}): Status => ({ installed: true, roles: ['agent'], readiness: unverified, execution_readiness: unverified, ...extra })
  assert.equal(availability(provider()), 'unverified')
  assert.equal(availability(provider({ installed: false })), 'missing')
  assert.equal(availability(provider({ readiness: { state: 'needs_setup', issue: { stage: 'authentication', code: 'sign_in_required' } } })), 'needs_setup')
  assert.equal(availability(provider({ readiness: { state: 'ready' } })), 'ready')
  assert.equal(availability(provider({ readiness: { state: 'starting' } })), 'starting')
  const roles: ProviderStatus['roles'][] = [['execution'], ['agent', 'execution'], ['execution', 'agent'], ['agent'], []]
  for (const role of roles) {
    const executionOnly = role.length === 1 && role[0] === 'execution'
    const status = provider({ roles: role, readiness: { state: 'ready' } })
    assert.equal(summaryReadiness(status), executionOnly ? status.execution_readiness : status.readiness)
    assert.equal(availability(status), executionOnly ? 'unverified' : 'ready')
    assert.equal(availability(provider({ roles: role, execution_readiness: { state: 'ready' } })), executionOnly ? 'ready' : 'unverified')
    assert.equal(availability(provider({ roles: role, installed: false, readiness: { state: 'ready' }, execution_readiness: { state: 'ready' } })), 'missing')
  }
  const botNeedsSetup = provider({ roles: ['agent', 'execution'], readiness: { state: 'needs_setup', issue: { stage: 'model_list', code: 'model_unavailable' } }, execution_readiness: { state: 'ready' } })
  assert.equal(summaryReadiness(botNeedsSetup), botNeedsSetup.readiness)
  assert.equal(availability(botNeedsSetup), 'needs_setup')
})

test('a provider shows whether it can be used before any use is picked', () => {
  const provider = (extra: Partial<ProviderStatus>) => ({ state: 'off', installed: true, env: [], ...extra }) as Pick<ProviderStatus, 'state' | 'installed' | 'env'>
  assert.equal(availability(provider({})), 'unverified')
  assert.equal(availability(provider({ installed: false })), 'missing')
  assert.equal(availability(provider({ env: [{ name: 'CURSOR_API_KEY', set: false, inherited: false, required: true }] })), 'needs_setup')
  assert.equal(availability(provider({ env: [{ name: 'CURSOR_API_KEY', set: false, inherited: true, required: true }] })), 'unverified')
  assert.equal(availability(provider({ state: 'starting' })), 'starting')
})

test('provider observations preserve unknown prerequisites and discard private fields', () => {
  const observed = {
    installed: { status: 'confirmed', evidence: 'binary_discovery' }, launchable: { status: 'confirmed', evidence: 'app_server_handshake' },
    authenticated: { status: 'unknown', evidence: 'authentication_not_observed' }, model_known: { status: 'unknown', evidence: 'model_catalog_not_observed' }, ready: { status: 'unknown', evidence: 'readiness_not_observed' },
  }
  const readiness = { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } }
  const provider = { provider: 'codex', roles: ['agent'], supports: ['agent', 'execution'], installed: true, bin: 'codex', app_server: null, setup: null, home: null, sessions: null, env: [], state: 'unverified', issues: [], readiness, execution_model: '', execution_readiness: readiness, probe: { provider: 'codex', config_id: 'config', checked_at_ms: null, launch: { state: 'unchecked' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null }, observed }
  const response = { settings: { roots: [], manager: null }, defaults: { workspace: '/project' }, readiness: { manager: readiness, missing: [] }, display_paths: {}, suggested_roots: [], providers: [provider] }
  const parsed = parseNodeSettings({ ...response, providers: [{ ...provider, observed: { ...observed, authenticated: { ...observed.authenticated, native_token: 'private-token' } } }] })
  assert.equal(parsed.providers[0].state, 'unverified')
  assert.deepEqual(parsed.providers[0].observed, observed)
  assert(!JSON.stringify(parsed).includes('private-token'))
  assert.throws(() => parseNodeSettings({ ...response, providers: [{ ...provider, observed: { ...observed, ready: { status: 'ready', evidence: 'readiness_not_observed' } } }] }))
  assert.throws(() => parseNodeSettings({ ...response, providers: [{ ...provider, observed: { ...observed, ready: { status: 'unknown', evidence: 'invented' } } }] }))
})

test('a task page shows a summary of what it was asked and folds the rest', () => {
  assert.deepEqual(promptSummary('修好登录页', '修好登录页'), { summary: '', folded: false }, 'a prompt that is its title adds nothing')
  assert.deepEqual(promptSummary('看看依赖', '检查依赖'), { summary: '看看依赖', folded: false })
  assert.deepEqual(promptSummary('## 目标\n把测试跑通\n细节…', '跑测试'), { summary: '目标', folded: true })
  assert.deepEqual(promptSummary('{"task":"build"}', '构建'), { summary: '', folded: true }, 'machine text only shows under 详细信息')
  const long = promptSummary('很'.repeat(100), '长任务')
  assert.equal(long.summary.length, 80)
  assert.equal(long.folded, true)
  assert.deepEqual(promptSummary('[紧急] 修好登录页', '修登录'), { summary: '[紧急] 修好登录页', folded: false }, 'plain words that open with a bracket still show')
  assert.deepEqual(promptSummary('[1, 2]', '列表'), { summary: '', folded: true }, 'a JSON array is machine text')
  assert.deepEqual(promptSummary('{"step":1}\n{"step":2}', '两步'), { summary: '', folded: true }, 'JSON lines are machine text too')
  const emoji = promptSummary('👨‍👩‍👧'.repeat(100), '一家人')
  assert.equal(emoji.summary, `${'👨‍👩‍👧'.repeat(79)}…`, 'the cut keeps whole emoji')
})

test('only fully marked latest summaries become semantic steps', () => {
  assert.deepEqual(parseSummarySteps('✓ 读记录\n→ 核对结果\n· 等待同步'), [
    { state: 'done', text: '读记录' }, { state: 'current', text: '核对结果' }, { state: 'pending', text: '等待同步' },
  ])
  assert.deepEqual(parseSummarySteps('✓ 完成\r\n·  后续'), [{ state: 'done', text: '完成' }, { state: 'pending', text: '后续' }])
  for (const text of ['', '普通摘要', '✓', '✓ ', '✓   ', '✓没有空格', '✓ 一步\n普通说明', '✓ 一步\n', '→\t一步']) assert.equal(parseSummarySteps(text), null, text)
})

test('scheduler observations distinguish waiting, stopping, publishing, history and unknown states', () => {
  assert.deepEqual(schedulerStatus('queued'), { label: '排队中', group: 'open' })
  assert.deepEqual(schedulerStatus('cancelling'), { label: '停止中', group: 'open' })
  assert.deepEqual(schedulerStatus('publishing'), { label: '回复同步中', group: 'open' })
  assert.deepEqual(schedulerStatus('reconciling'), { label: '正在查询上次回复', group: 'open' })
  assert.equal(schedulerStatus('completed').group, 'history')
  assert.deepEqual(schedulerStatus('future-state'), { label: '状态未知', group: 'unknown' })
})

test('losing the entry makes active tasks stale without claiming every target is offline', () => {
  const work: WorkRecord = { work_id: 'w', trigger: '', target_node: 'mac', cwd: '', title: '', prompt: '', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null, resolved: { provider: 'codex', model: null } }
  const state = cardState(work, [unready], { state: 'running', outcome: null })
  assert.equal(state, 'running', 'the last target report is still online')
  assert.equal(taskStale(state, false), true)
  assert.equal(staleTaskLabel, '状态未知')
  assert.equal(taskGroup(taskStale(state, false) ? 'unknown' : state, false), 'attention')
  assert.equal(taskStale('completed', false), false)
  assert.equal(externalSessionLabels[externalSessionStatus({ node: 'mac' }, [{ ...unready, revoked: true }])], '设备已撤权')
  assert.equal(externalSessionStatus({ node: 'mac' }, [{ ...unready, online: false }]), 'offline')
  assert.equal(externalSessionStatus({ node: 'other' }, [unready]), 'unknown')
})

test('manager attention keeps its notification identity and can be hidden locally', () => {
  const attention = source(true)
  const bot = { bot_id: 'b', conversation_id: 'c', name: '助手' } as unknown as BotRecord
  const note = { notification_id: 'n1', kind: 'manager_attention', text: '看看进展', conversation_id: 'c' } satisfies NotificationRecord
  attention.workspace.bots = [bot]
  attention.workspace.notifications = [note]
  const item = inboxItems(attention).find((candidate) => candidate.kind === 'attention')
  assert.equal(item?.kind, 'attention')
  if (item?.kind === 'attention') assert.equal(item.notification.notification_id, 'n1')
  attention.workspace.ignoredNotifications = new Set(['n1'])
  assert.equal(inboxItems(attention).some((candidate) => candidate.kind === 'attention'), false)
})
