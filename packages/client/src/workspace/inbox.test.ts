import assert from 'node:assert/strict'
import test from 'node:test'
import type { JoinRequest } from '../lib/node.ts'
import { inboxItems, waitingItems, type InboxSource } from './inbox.ts'
import { deviceForm, deviceOnline, formName } from './labels.ts'
import type { BotRecord, NodeInfo, NotificationRecord, Question } from './types.ts'

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
})

test('a phone asking to join reads as a phone, and only a computer has an online state', () => {
  assert.equal(formName[deviceForm('client', join('pending').name)], '手机')
  assert.equal(formName[deviceForm('client', 'Chrome')], '浏览器')
  assert.equal(formName[deviceForm('client', '我的 iPad')], '平板')
  assert.equal(formName[deviceForm('node', '我的手机')], '电脑')
  assert.equal(deviceOnline('client', undefined, false, true), undefined, 'no device knows whether a browser on another connection is open')
  assert.equal(deviceOnline('node', { online: false }, true, true), false)
  assert.equal(deviceOnline('node', undefined, true, true), true, 'the connected computer is online before the device list arrives')
  assert.equal(deviceOnline('node', { online: true }, true, false), false, 'the computer this device lost reads offline, not its last report')
  assert.equal(deviceOnline('node', { online: true }, false, false), undefined, 'another computer is unknown while this device is offline')
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
