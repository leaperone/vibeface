import assert from 'node:assert/strict'
import test from 'node:test'
import { accessOf, returnPath } from './access.ts'

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
