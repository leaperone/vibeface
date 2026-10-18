import assert from 'node:assert/strict'
import test from 'node:test'
import { primaryLane, shouldAutoConnect } from './connection-lane.ts'

test('one saved entry connects itself, and several entries stay listed', () => {
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 1 }), true)
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 2 }), false)
  assert.equal(shouldAutoConnect({ hasInvitation: false, savedCount: 0 }), false)
  assert.equal(shouldAutoConnect({ hasInvitation: true, savedCount: 1 }), false)
  assert.equal(primaryLane({ hasInvitation: true, savedCount: 3 }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 0 }), 'invite')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 1 }), 'saved')
  assert.equal(primaryLane({ hasInvitation: false, savedCount: 2 }), 'saved')
})
