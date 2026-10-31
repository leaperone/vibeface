import assert from 'node:assert/strict'
import test from 'node:test'
import { availability, type JoinRequest, type ProviderStatus } from '../lib/node.ts'
import { inboxItems, waitingItems, type InboxSource } from './inbox.ts'
import { cardState, chatTaskState, deviceForm, deviceOnline, entryLabel, externalSessionLabels, externalSessionStatus, fixOnDevice, formName, parseSummarySteps, promptSummary, schedulerStatus, staleTaskLabel, taskGroup, taskLabel, taskStale } from './labels.ts'
import type { NodeInfo, Question, WorkRecord } from './types.ts'

const question = (id: string, closed = false): Question => ({
  question_id: id, conversation_id: 'c', bot: { bot_id: 'b', name: '助手' }, severity: 'normal', text: id, choices: [], about: { kind: 'general' },
  author: 'node', created_at_ms: 1, answer: null, closed: closed ? { reason: 'expired', at_ms: 2 } : null,
} as unknown as Question)
const join = (status: JoinRequest['status']): JoinRequest => ({
  join_id: status, status, network_id: 'n', invite_id: 'i', device_id: 'phone', kind: 'client', name: '我的手机', commit: 'c',
  admin_id: null, admin_nonce: null, joiner_nonce: null, reason: null, expires_at_ms: 1,
})
const unready: NodeInfo = { device_id: 'mac', name: 'Mac', local: true, online: true, revoked: false, capabilities: { manager: 'codex', manager_state: 'needs_setup', relay: false, roots: [] } }

const source = (manage: boolean, answered: Record<string, string> = {}): InboxSource => ({
  device: { ready: { principal: { network_id: 'n', device_id: 'me', epoch: 1, source: 'membership', can_manage: manage }, entry: { device_id: 'mac' } }, joins: [join('pending'), join('joined')], pendingRecovery: null, rotateHint: false },
  workspace: {
    writeState: { state: 'writable', conflicts: [] }, questions: [question('open'), question('closed', true)], answered, openWorks: [], nodes: [unready], data: null,
    messagePending: null, messageBusy: false, selectedBot: null, notifications: [], bots: [],
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
})

test('a failed task waits in 待处理 under its bot until something follows it up, naming its device', () => {
  const failed = source(false)
  const work = { work_id: 'w1', conversation_id: 'c', trigger: '', target_node: 'mac', cwd: '/w', title: '跑测试', prompt: '', state: 'finished', summary: '', outcome: 'failed', result_text: '', failure_reason: 'timed out' } as const
  failed.workspace.openWorks = [work, { ...work, work_id: 'w2', outcome: null, state: 'running' }, { ...work, work_id: 'w3', title: '', outcome: null, state: 'dispatch_rejected' }]
  failed.workspace.bots = [{ bot_id: 'b', conversation_id: 'c', name: '助手' } as never]
  assert.deepEqual(inboxItems(failed).filter((item) => item.kind === 'failed').map((item) => [item.key, item.title, item.meta]), [['failed:w1', '跑测试', 'Mac'], ['failed:w3', '任务没有完成', 'Mac']], 'a refused dispatch waits like a failure')
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
  const work = (work_id: string, extra: Partial<WorkRecord> = {}): WorkRecord => ({ work_id, trigger: '', target_node: 'mac', cwd: '/w', title: work_id, prompt: '', state: 'finished', summary: '', outcome: 'completed', result_text: '', failure_reason: null, ...extra })
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

test('a provider shows whether it can be used before any use is picked', () => {
  const provider = (extra: Partial<ProviderStatus>) => ({ state: 'off', installed: true, env: [], ...extra }) as Pick<ProviderStatus, 'state' | 'installed' | 'env'>
  assert.equal(availability(provider({})), 'ready')
  assert.equal(availability(provider({ installed: false })), 'missing')
  assert.equal(availability(provider({ env: [{ name: 'CURSOR_API_KEY', set: false, inherited: false, required: true }] })), 'needs_setup')
  assert.equal(availability(provider({ env: [{ name: 'CURSOR_API_KEY', set: false, inherited: true, required: true }] })), 'ready')
  assert.equal(availability(provider({ state: 'starting' })), 'starting')
})

test('a task page shows a summary of what it was asked and folds the rest', () => {
  assert.deepEqual(promptSummary('修好登录页', '修好登录页'), { summary: '', folded: false }, 'a prompt that is its title adds nothing')
  assert.deepEqual(promptSummary('看看依赖', '检查依赖'), { summary: '看看依赖', folded: false })
  assert.deepEqual(promptSummary('## 目标\n把测试跑通\n细节…', '跑测试'), { summary: '目标', folded: true })
  assert.deepEqual(promptSummary('{"task":"build"}', '构建'), { summary: '', folded: true }, 'machine text only shows under 详细信息')
  const long = promptSummary('很'.repeat(100), '长任务')
  assert.equal(long.summary.length, 80)
  assert.equal(long.folded, true)
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
  assert.equal(schedulerStatus('completed').group, 'history')
  assert.deepEqual(schedulerStatus('future-state'), { label: '状态未知', group: 'unknown' })
})

test('losing the entry makes active tasks stale without claiming every target is offline', () => {
  const work: WorkRecord = { work_id: 'w', trigger: '', target_node: 'mac', cwd: '', title: '', prompt: '', state: 'running', summary: '', outcome: null, result_text: '', failure_reason: null }
  const state = cardState(work, [unready], 'running')
  assert.equal(state, 'running', 'the last target report is still online')
  assert.equal(taskStale(state, false), true)
  assert.equal(staleTaskLabel, '状态未知')
  assert.equal(taskGroup(taskStale(state, false) ? 'unknown' : state, false), 'attention')
  assert.equal(taskStale('completed', false), false)
  assert.equal(externalSessionLabels[externalSessionStatus({ node: 'mac' }, [{ ...unready, revoked: true }])], '设备已撤权')
  assert.equal(externalSessionStatus({ node: 'mac' }, [{ ...unready, online: false }]), 'offline')
  assert.equal(externalSessionStatus({ node: 'other' }, [unready]), 'unknown')
})
