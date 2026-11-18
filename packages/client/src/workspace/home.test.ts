import assert from 'node:assert/strict'
import test from 'node:test'
import { backTo, byRecent, emptyHome, homeBot, mainConversationAttention, unreachable } from './home.ts'
import { sessionNotificationKind, type BotRecord, type NotificationRecord, type Question } from './types.ts'

const bot = (id: string, at?: number): BotRecord => ({ bot_id: id, conversation_id: `c-${id}`, name: id, instructions: '', models: {}, skills: null, avatar: null, avatar_version: null, event_hash: id, last_message: at === undefined ? null : { text: 'hi', role: 'assistant', created_at_ms: at }, busy: null })
const bots = [bot('a', 1), bot('b', 3), bot('c')]

test('a phone opens to the conversation list', () => {
  assert.equal(homeBot({ narrow: true, bots, current: 'c-b' }), null)
  assert.equal(homeBot({ narrow: true, conversation: 'gone', bots, current: 'c-b' }), null)
})

test('a notification link opens its conversation on any screen', () => {
  assert.equal(homeBot({ narrow: true, conversation: 'c-c', bots, current: 'c-a' })?.bot_id, 'c')
  assert.equal(homeBot({ narrow: false, conversation: 'c-c', bots, current: 'c-a' })?.bot_id, 'c')
})

test('a wide screen opens the last conversation, else the first', () => {
  assert.equal(homeBot({ narrow: false, bots, current: 'c-b' })?.bot_id, 'b')
  assert.equal(homeBot({ narrow: false, bots, current: '' })?.bot_id, 'a')
  assert.equal(homeBot({ narrow: false, bots: [], current: '' }), null)
})

test('the list puts the newest message first and silent bots last', () => {
  assert.deepEqual(byRecent(bots).map((item) => item.bot_id), ['b', 'a', 'c'])
})

test('main Bot attention matches both IDs, keeps waiting questions, and only shows session previews when supported', () => {
  const main = { kind: 'bot_main', bot_id: 'a' } satisfies NotificationRecord['target']
  const task = { kind: 'task', task_id: 'task' } satisfies NotificationRecord['target']
  const note = (id: string, extra: Partial<NotificationRecord> = {}): NotificationRecord =>
    ({ notification_id: id, kind: 'manager_response', text: id, conversation_id: 'c-a', target: main, ...extra })
  const question = (id: string, extra: Partial<Question> = {}): Question => ({
    question_id: id, conversation_id: 'c-a', target: main, bot: null, severity: 'attention', text: id, choices: [],
    about: { node: 'node', agent: null, session_id: 's', cwd: '', summary: '' }, author: 'node', created_at_ms: 1,
    answer: null, closed: null, ...extra,
  })
  const reply = note('reply')
  const otherReply = note('other-reply', { conversation_id: 'c-b', target: { kind: 'bot_main', bot_id: 'b' } })
  const session = note('session', { kind: sessionNotificationKind })
  const input = {
    bots: [...bots, bots[0], { ...bots[0], conversation_id: 'c-old' }],
    notifications: [
      reply, otherReply, session, note('ignored'), note('wrong-bot', { target: { kind: 'bot_main', bot_id: 'b' } }),
      note('wrong-chat', { conversation_id: 'c-b' }), note('missing-target', { target: undefined }), note('task', { target: task }),
      note('old-chat', { conversation_id: 'c-old' }),
    ],
    questions: [
      question('open'), question('answered-here'), question('answered-elsewhere', { answer: { message_id: 'answer', text: '继续', author: 'other', created_at_ms: 2 } }),
      question('closed', { closed: 'request_lost' }), question('wrong-chat', { conversation_id: 'c-b' }),
      question('wrong-bot', { target: { kind: 'bot_main', bot_id: 'b' } }), question('task', { target: task }), question('missing-target', { target: undefined }),
    ],
    answered: { 'answered-here': '继续' },
    ignoredNotifications: new Set(['ignored']),
  }
  const web = mainConversationAttention({ ...input, includeSessionNotifications: true })
  const mobile = mainConversationAttention({ ...input, includeSessionNotifications: false })
  assert.deepEqual(web.unread, { 'c-a': 2, 'c-b': 1, 'c-old': 1 })
  assert.deepEqual(mobile.unread, web.unread)
  assert.equal(web.latestNotification.get('c-a'), session)
  assert.equal(mobile.latestNotification.get('c-a'), reply)
  assert.equal(web.latestNotification.get('c-b'), otherReply)
  assert.equal(mobile.latestNotification.get('c-b'), otherReply)
  assert.equal(web.latestNotification.has('c-c'), false)

  const repeatedReply = { ...reply, text: 'latest text' }
  const repeated = mainConversationAttention({ ...input, notifications: [reply, repeatedReply], questions: [], includeSessionNotifications: true })
  assert.deepEqual(repeated.unread, { 'c-a': 2 })
  assert.equal(repeated.latestNotification.get('c-a'), repeatedReply)
})

test('on a phone every page below a tab leads back toward it', () => {
  for (const tab of ['/', '/tasks', '/settings']) assert.equal(backTo(tab), null, tab)
  for (const [from, to] of [['/b/bot', '/'], ['/b/bot/work/w1', '/b/bot'], ['/channels', '/'], ['/tasks/t1', '/tasks'], ['/devices', '/settings'], ['/devices/d1', '/devices'], ['/settings/logs', '/settings'], ['/settings/lab', '/settings'], ['/settings/lab/transfers', '/settings/lab']]) assert.equal(backTo(from), to, from)
  assert.equal(backTo('/devices/d1', 'bot'), '/b/bot')
  assert.equal(backTo('/settings', 'bot'), null)
})

test('an empty list tells a network it cannot reach apart from one still loading', () => {
  assert.equal(emptyHome({ synced: false, link: { kind: 'connecting', attempt: 0 } }), 'loading')
  assert.equal(emptyHome({ synced: false, link: { kind: 'online', catchingUp: true } }), 'loading')
  assert.equal(emptyHome({ synced: false, link: { kind: 'offline', attempt: 1, retryInMs: 500, reason: '' } }), 'unreachable')
  assert.equal(emptyHome({ synced: false, link: { kind: 'connecting', attempt: 2 } }), 'unreachable')
  assert.equal(emptyHome({ synced: true, link: { kind: 'online', catchingUp: false } }), 'empty')
})

test('a network counts as unreachable from the first failed attempt until one gets through', () => {
  assert.equal(unreachable({ kind: 'connecting', attempt: 0 }), false)
  assert.equal(unreachable({ kind: 'offline', attempt: 1, retryInMs: 500, reason: '' }), true)
  assert.equal(unreachable({ kind: 'connecting', attempt: 1 }), true)
  assert.equal(unreachable({ kind: 'online', catchingUp: true }), false)
})
