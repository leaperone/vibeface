import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { colors, eyePaths, faceOf, HUES, mix, pose, thinkingDots, type Mood, type Outline } from './face.ts'
import { avatarChangeWords, avatarWords, avatarWrite, liveLook, type Look } from './pick.ts'

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

test('the id gives the same outline, eyes and hue as the Node derives, every hue included', () => {
  const vectors: { bot_id: string; outline: string; eyes: string; hue: number }[] = JSON.parse(readFileSync(new URL('./derived-faces.json', import.meta.url), 'utf8'))
  for (const { bot_id, ...expected } of vectors) {
    const { outline, eyes, hue } = faceOf(bot_id)
    assert.deepEqual({ outline, eyes, hue }, expected, bot_id)
  }
  assert.deepEqual([...new Set(vectors.map(vector => vector.hue))].sort((a, b) => a - b), [...HUES])
  assert.deepEqual(new Set(vectors.map(vector => vector.outline)), new Set(outlines))
  assert.deepEqual(new Set(vectors.map(vector => vector.eyes)), new Set(['dot', 'pill', 'oval', 'wide']))
})

test('a pending panel write goes again only while the face is at the version it was signed over', () => {
  const cloud = { outline: 'cloud', eyes: 'wide', hue: 285 } as const
  const bean = { outline: 'bean', eyes: 'dot', hue: 155 } as const
  assert.equal(avatarWrite({ avatar: cloud, avatar_version: 'v1' }, null, cloud), 'landed')
  assert.equal(avatarWrite({ avatar: null, avatar_version: null }, null, cloud), 'unsent')
  assert.equal(avatarWrite({ avatar: cloud, avatar_version: 'v1' }, 'v1', null), 'unsent')
  assert.equal(avatarWrite({ avatar: null, avatar_version: 'v2' }, null, cloud), 'superseded', 'the face went back, yet its version moved on')
  assert.equal(avatarWrite({ avatar: bean, avatar_version: 'v3' }, null, cloud), 'superseded')
})

test('a written pick stays on show until the face has a new version, and never on another bot', () => {
  const bean = { outline: 'bean', eyes: 'dot', hue: 155 } as const
  const writing: Look = { bot_id: 'a', avatar: bean, version: null, writing: true }
  const written = { ...writing, writing: false }
  assert.equal(liveLook(writing, { bot_id: 'a', avatar: null, avatar_version: 'v1' }), writing, 'kept while the Node has not answered')
  assert.equal(liveLook(written, { bot_id: 'a', avatar: null, avatar_version: null }), written, 'a list not yet refreshed keeps the pick')
  assert.equal(liveLook(written, { bot_id: 'a', avatar: bean, avatar_version: 'v1' }), null, 'the list shows the pick')
  assert.equal(liveLook(written, { bot_id: 'a', avatar: null, avatar_version: 'v2' }), null, 'a later change elsewhere shows, even back to the old face')
  assert.equal(liveLook(writing, { bot_id: 'b', avatar: null, avatar_version: null }), null, 'another bot starts clean')
})

test('a face change reads out in words, the id\'s face named too', () => {
  const derived = faceOf('bot-a')
  assert.equal(avatarChangeWords('bot-a', { outline: 'round', eyes: 'dot', hue: 255 }, { outline: 'cloud', eyes: 'wide', hue: 285 }), '长相：圆、圆点眼、蓝色 → 云朵、大眼、紫色')
  assert.equal(avatarChangeWords('bot-a', { outline: 'round', eyes: 'dot', hue: 255 }, null), `长相：圆、圆点眼、蓝色 → ${avatarWords(derived)}（默认）`)
})
