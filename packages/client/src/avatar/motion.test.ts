import assert from 'node:assert/strict'
import test from 'node:test'
import { faceMotion } from './motion.ts'

test('a mood change blends from the pose on screen and settles on the new mood', () => {
  const motion = faceMotion('bot-a', 'idle')
  const awake = motion.frame(1000)
  motion.follow('asleep')
  const first = motion.frame(1016)
  assert.ok(first.grey < 0.1, `starts from the coloured face, grey ${first.grey}`)
  const middle = motion.frame(1016 + 120)
  assert.ok(middle.grey > first.grey && middle.grey < 1, `fades over the blend, grey ${middle.grey}`)
  assert.equal(motion.frame(1016 + 420).grey, 1)
  assert.equal(awake.ringOpacity, 0)
})

test('the still pose ends a blend at once', () => {
  const motion = faceMotion('bot-a', 'idle')
  motion.frame(null)
  motion.follow('waiting')
  assert.equal(motion.frame(null).ringOpacity, 1)
})
