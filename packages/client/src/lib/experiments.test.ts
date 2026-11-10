import assert from 'node:assert/strict'
import test from 'node:test'
import { experimentsOn, setExperiments } from './experiments.ts'
import type { KeyValue } from './platform.ts'
import { storageOf } from '../workspace/storage.ts'

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

test('shared JSON storage preserves device keys and optional read fallbacks', () => {
  const storage = memory()
  const { readStorage, writeStorage } = storageOf(storage)
  writeStorage('connections', [{ device_id: 'node' }], true)
  assert.equal(storage.getItem('vibeface.connections'), '[{"device_id":"node"}]')
  assert.deepEqual(readStorage<unknown>('connections', null), [{ device_id: 'node' }])
  storage.setItem('vibeface.broken', '{')
  assert.equal(readStorage('broken', null), null)
  assert.throws(() => readStorage('broken', null, true), { message: '读不了本地需要确认的记录，已暂停发送。先别清除这台设备上的应用数据。' })
  assert.equal(readStorage('missing', null, true), null)
  for (const value of [false, 0, '']) {
    writeStorage('value', value, true)
    assert.equal(readStorage<unknown>('value', null), value)
  }
})

test('required JSON reads and writes stop when local storage is unavailable', () => {
  const { readStorage, writeStorage } = storageOf({ getItem() { throw new Error('blocked') }, setItem() { throw new Error('blocked') } })
  assert.equal(readStorage('pending-join', null), null)
  assert.throws(() => readStorage('sequence', 0, true), { message: '读不了本地需要确认的记录，已暂停发送。先别清除这台设备上的应用数据。' })
  assert.throws(() => writeStorage('sequence', 1, true), { message: '保存不了这条记录。允许保存本地数据后重试。' })
})
