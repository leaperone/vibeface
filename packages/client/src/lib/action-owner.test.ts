import assert from 'node:assert/strict'
import test from 'node:test'
import { createActionOwner } from './action-owner.ts'

test('only a current action can claim or deliberately follow its generation transition', () => {
  const owner = createActionOwner<number>()
  let generation = 1
  const current = (value: number) => value === generation
  const action = owner.claim(generation, current)
  assert.ok(action)
  assert.equal(owner.claim(generation, current), null)
  generation++
  assert.equal(action.current(), false)
  assert.equal(action.rebind({ from: 0, to: generation }), false)
  assert.equal(action.rebind({ from: 1, to: 1 }), false)
  assert.equal(action.rebind({ from: 1, to: generation }), true)
  assert.equal(action.current(), true)
  assert.equal(owner.busy(), true)
  assert.equal(action.finish(), true)
  assert.equal(owner.busy(), false)
})

test('stale and cancelled claims cannot transfer or finish a replacement', () => {
  const owner = createActionOwner<number>()
  let generation = 1
  const current = (value: number) => value === generation
  const stale = owner.claim(generation, current)
  assert.ok(stale)
  generation++
  assert.equal(owner.claim(1, current), null)
  const cancelled = owner.claim(generation, current)
  assert.ok(cancelled)
  owner.cancel()
  const replacement = owner.claim(generation, current)
  assert.ok(replacement)
  assert.equal(stale.rebind({ from: 1, to: generation }), false)
  for (const earlier of [stale, cancelled]) {
    assert.equal(earlier.current(), false)
    assert.equal(earlier.finish(), false)
  }
  assert.equal(replacement.current(), true)
  assert.equal(owner.busy(), true)
  assert.equal(replacement.finish(), true)
})
