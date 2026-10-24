import assert from 'node:assert/strict'
import test from 'node:test'
import { recoveryCheck } from './recovery-notice.ts'

const checkpoint = (recovery_key: string, recovered: boolean) => ({ recovery_key, directory: recovered ? { members: [] } : null })
const recovered = { checkpoints: [checkpoint('k1', true)] }

test('a recovery since the key this browser accepted asks the person', () => {
  assert.equal(recoveryCheck(recovered, 'net', 'k1', { seen: 'net', own: null }), 'notice')
  assert.equal(recoveryCheck({ checkpoints: [checkpoint('k1', true), checkpoint('k2', false)] }, 'net', 'k2', { seen: 'net', own: null }), 'notice')
  assert.equal(recoveryCheck(recovered, 'net', 'k1', { seen: 'k1', own: null }), 'same')
})

test('a network seen for the first time is only recorded', () => {
  assert.equal(recoveryCheck(recovered, 'net', 'k1', { seen: null, own: null }), 'adopt')
})

test('a change this browser made, or a new recovery password, is adopted quietly', () => {
  assert.equal(recoveryCheck(recovered, 'net', 'k1', { seen: 'net', own: 'k1' }), 'adopt')
  assert.equal(recoveryCheck({ checkpoints: [checkpoint('k1', false)] }, 'net', 'k1', { seen: 'net', own: null }), 'adopt')
  assert.equal(recoveryCheck({ checkpoints: [checkpoint('k1', true), checkpoint('k2', false)] }, 'net', 'k2', { seen: 'net', own: 'k1' }), 'adopt')
})
