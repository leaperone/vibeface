import assert from 'node:assert/strict'
import test from 'node:test'
import { backTo, byRecent, homeBot } from './home.ts'
import type { BotRecord } from './types.ts'

const bot = (id: string, at?: number): BotRecord => ({ bot_id: id, conversation_id: `c-${id}`, name: id, instructions: '', models: {}, skills: null, event_hash: id, last_message: at === undefined ? null : { text: 'hi', role: 'assistant', created_at_ms: at }, busy: null })
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

test('on a phone every page leads back toward the list', () => {
  assert.equal(backTo('/'), null)
  for (const [from, to] of [['/b/bot', '/'], ['/b/bot/work/w1', '/b/bot'], ['/inbox', '/'], ['/devices', '/'], ['/devices/d1', '/devices'], ['/settings', '/'], ['/settings/logs', '/settings']]) assert.equal(backTo(from), to, from)
  assert.equal(backTo('/devices/d1', 'bot'), '/b/bot')
  assert.equal(backTo('/settings', 'bot'), '/')
})
