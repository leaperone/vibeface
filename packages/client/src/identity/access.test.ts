import assert from 'node:assert/strict'
import test from 'node:test'
import { accessOf, returnPath } from './access.ts'
import { hoursUntil, recoveryCopy, whenText } from './copy.ts'

const browser = { ready: false, phase: 'disconnected', joining: false, savedEntries: 0, invitation: false, offlineRetryMs: null }

test('a browser without a membership goes to the connect page', () => {
  for (const phase of ['disconnected', 'unpaired', 'removed', 'key_lost', 'blocked']) assert.deepEqual(accessOf({ ...browser, phase }), { kind: 'auth' }, phase)
  assert.deepEqual(accessOf({ ...browser, phase: 'connecting', savedEntries: 1, invitation: true }), { kind: 'auth' })
})

test('saved credentials that are still reconnecting or offline keep the page, never the connect page', () => {
  assert.deepEqual(accessOf({ ...browser, phase: 'reconnecting', savedEntries: 1 }), { kind: 'resuming', retryInSeconds: null })
  assert.deepEqual(accessOf({ ...browser, phase: 'reconnecting', savedEntries: 1, offlineRetryMs: 4200 }), { kind: 'resuming', retryInSeconds: 5 })
  assert.deepEqual(accessOf({ ...browser, phase: 'connecting', savedEntries: 1 }), { kind: 'loading' })
  assert.deepEqual(accessOf({ ...browser, phase: 'initializing' }), { kind: 'loading' })
  assert.deepEqual(accessOf({ ...browser, ready: true, phase: 'ready', savedEntries: 1 }), { kind: 'member' })
})

test('the connect page returns only to a page of this site', () => {
  assert.equal(returnPath('/b/bot?x=1'), '/b/bot?x=1')
  assert.equal(returnPath('/?conversation=c'), '/?conversation=c')
  for (const unsafe of [undefined, '', 'https://evil.example', '//evil.example', '/\\evil.example', '/connect?redirect=/inbox']) assert.equal(returnPath(unsafe), '/', String(unsafe))
})

test('a recovery wait reads in whole hours and as a day people say', () => {
  const now = new Date(2026, 9, 27, 22, 30).getTime()
  const hour = 3_600_000
  assert.equal(hoursUntil(now + 47.2 * hour, now), 48)
  assert.equal(hoursUntil(now + 60_000, now), 1)
  assert.equal(recoveryCopy.left(now + 2 * hour, now), '剩余 2 小时')
  assert.equal(recoveryCopy.left(now - 1, now), '马上生效')
  assert.match(recoveryCopy.confirmNote, /管理设备/)
  assert.match(recoveryCopy.confirmNote, /不可撤销/)
  assert.equal(whenText(new Date(2026, 9, 27, 23, 5).getTime(), now), '今天 23:05')
  assert.equal(whenText(new Date(2026, 9, 28, 9, 0).getTime(), now), '明天 09:00')
  assert.equal(whenText(new Date(2026, 9, 29, 22, 30).getTime(), now), '后天 22:30')
  assert.equal(whenText(new Date(2026, 9, 30, 8, 0).getTime(), now), '10月30日 08:00')
})
