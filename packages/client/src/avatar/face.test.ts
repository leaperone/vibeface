import assert from 'node:assert/strict'
import test from 'node:test'
import { colors, eyePaths, faceOf, mix, pose, thinkingDots, type Mood, type Outline } from './face.ts'

const outlines: Outline[] = ['round', 'squircle', 'drop', 'bean', 'cloud']
const moods: Mood[] = ['idle', 'thinking', 'replying', 'waiting', 'asleep']

test('a bot keeps its face, and no face takes the halo amber', () => {
  assert.deepEqual(faceOf('bot-a'), faceOf('bot-a'))
  const hues = new Set(Array.from({ length: 400 }, (_, i) => faceOf(`bot-${i}`).hue))
  assert.equal(hues.size, 12)
  for (const hue of hues) assert.ok(hue < 60 || hue > 90, `hue ${hue}`)
})

test('ids spread over every outline and eye shape', () => {
  const faces = Array.from({ length: 200 }, (_, i) => faceOf(`bot-${i}-${i * 7919}`))
  assert.deepEqual(new Set(faces.map(face => face.outline)), new Set(outlines))
  assert.deepEqual(new Set(faces.map(face => face.eyes)), new Set(['dot', 'pill', 'oval', 'wide']))
  for (const face of faces) {
    assert.ok(face.gap >= 10 && face.gap <= 14, `gap ${face.gap}`)
    assert.ok(face.tilt >= -6 && face.tilt <= 6, `tilt ${face.tilt}`)
  }
})

test('every outline and mood stays finite and inside the radius band', () => {
  for (const outline of outlines) for (const eyes of ['dot', 'pill', 'oval', 'wide'] as const) {
    const face = { ...faceOf(outline), outline, eyes }
    for (const mood of moods) for (let t = 0; t <= 20000; t += 37) {
      const p = pose(face, mood, t)
      for (const value of Object.values(p).flat()) assert.ok(Number.isFinite(value), `${outline} ${mood} ${t}`)
      assert.ok(p.radii.every(r => r >= 0.85 && r <= 1.15), `${outline} ${mood} ${t}`)
      assert.ok(eyePaths(face, p).every(d => !d.includes('NaN')), `${eyes} ${mood} ${t}`)
      const dots = thinkingDots(p, t)
      assert.ok(dots.every(dot => (mood === 'thinking' ? dot.opacity > 0.3 : dot.opacity === 0) && Number.isFinite(dot.x)), `${mood} ${t}`)
    }
  }
})

test('mix lands exactly on either end', () => {
  const face = faceOf('bot-a')
  const a = pose(face, 'idle', 1234)
  const b = pose(face, 'replying', 4321)
  assert.deepEqual(mix(a, b, 0), a)
  assert.deepEqual(mix(a, b, 1), b)
})

test('every hue gives in-gamut hex colours in both themes, asleep included', () => {
  const hues = new Set(Array.from({ length: 400 }, (_, i) => faceOf(`bot-${i}`).hue))
  for (const hue of hues) for (const dark of [false, true]) for (const saturation of [1, 0.35]) {
    for (const value of Object.values(colors({ ...faceOf('bot-a'), hue }, dark, saturation))) assert.match(value, /^#[0-9a-f]{6}$/)
  }
})

test('a picked avatar replaces outline, eyes and hue but keeps the id\'s spacing, tilt and rhythm', () => {
  const derived = faceOf('bot-a')
  const picked = faceOf('bot-a', { outline: 'cloud', eyes: 'wide', hue: 345 })
  assert.deepEqual(picked, { ...derived, outline: 'cloud', eyes: 'wide', hue: 345 })
  assert.deepEqual(faceOf('bot-a', null), derived)
  for (let t = 0; t <= 20000; t += 50) assert.equal(pose(picked, 'idle', t).open, pose(derived, 'idle', t).open, `blink at ${t}`)
})
