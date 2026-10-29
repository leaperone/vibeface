import assert from 'node:assert/strict'
import test from 'node:test'
import { experimentsOn, setExperiments } from './experiments.ts'
import type { KeyValue } from './platform.ts'

const memory = (): KeyValue => { const values = new Map<string, string>(); return { getItem: (key) => values.get(key) ?? null, setItem: (key, value) => { values.set(key, value) } } }

test('实验功能 is off until turned on, and stays as set across restarts', () => {
  const storage = memory()
  assert.equal(experimentsOn(storage), false)
  setExperiments(storage, true)
  assert.equal(experimentsOn(storage), true)
  setExperiments(storage, false)
  assert.equal(experimentsOn(storage), false)
  assert.equal(experimentsOn({ getItem: () => { throw new Error('blocked') }, setItem() {} }), false, 'unreadable storage keeps it off')
})
