import assert from 'node:assert/strict'
import test from 'node:test'
import type { JoinRequest } from '../lib/node.ts'
import { inboxItems, waitingItems, type InboxSource } from './inbox.ts'
import { deviceForm, deviceOnline, formName } from './labels.ts'
import type { NodeInfo, Question } from './types.ts'

const question = (id: string, closed = false): Question => ({
  question_id: id, conversation_id: 'c', bot: { bot_id: 'b', name: '助手' }, severity: 'normal', text: id, choices: [], about: { kind: 'general' },
  author: 'node', created_at_ms: 1, answer: null, closed: closed ? { reason: 'expired', at_ms: 2 } : null,
} as unknown as Question)
const join = (status: JoinRequest['status']): JoinRequest => ({
  join_id: status, status, network_id: 'n', invite_id: 'i', device_id: 'phone', kind: 'client', name: '我的手机', commit: 'c',
  admin_id: null, admin_nonce: null, joiner_nonce: null, reason: null, expires_at_ms: 1,
})
const unready: NodeInfo = { device_id: 'mac', name: 'Mac', local: true, online: true, revoked: false, capabilities: { manager: 'codex', manager_ready: false, relay: false, roots: [] } }

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
})

test('a phone asking to join reads as a phone, and only a computer has an online state', () => {
  assert.equal(formName[deviceForm('client', join('pending').name)], '手机')
  assert.equal(formName[deviceForm('client', 'Chrome')], '浏览器')
  assert.equal(formName[deviceForm('node', '我的手机')], '电脑')
  assert.equal(deviceOnline('client', undefined, false), undefined, 'no device knows whether a browser on another connection is open')
  assert.equal(deviceOnline('node', { online: false }, true), false)
  assert.equal(deviceOnline('node', undefined, true), true, 'the connected computer is online before the device list arrives')
})
