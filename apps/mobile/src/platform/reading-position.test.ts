import assert from 'node:assert/strict'
import test from 'node:test'
import { readingPositions } from './reading-position.ts'

const storage = () => {
  const items = new Map<string, string>()
  return { items, getItem: (key: string) => items.get(key) ?? null, setItem: (key: string, value: string) => { items.set(key, value) } }
}

test('reading anchors survive reopening and remain isolated by network, device and conversation', () => {
  const store = storage(), identity = { networkId: 'network', deviceId: 'phone' }
  readingPositions(store, identity).save('a', { kind: 'message', messageId: 'old-message' })
  readingPositions(store, identity).save('b', { kind: 'latest' })
  const reopened = readingPositions(store, identity)
  assert.deepEqual(reopened.get('a'), { kind: 'message', messageId: 'old-message' })
  assert.deepEqual(reopened.get('b'), { kind: 'latest' })
  assert.equal(readingPositions(store, { ...identity, networkId: 'other' }).get('a'), null)
  assert.equal(readingPositions(store, { ...identity, deviceId: 'other' }).get('a'), null)
  reopened.clear()
  assert.equal(reopened.get('a'), null)
})

test('malformed storage cannot provide navigation targets and old anchors are bounded', () => {
  const store = storage(), positions = readingPositions(store, { networkId: 'network', deviceId: 'phone' })
  positions.save('a', { kind: 'latest' })
  const key = [...store.items.keys()][0]
  assert.ok(key)
  store.items.set(key, '[{"conversationId":"a","position":{"kind":"message","messageId":12}}]')
  assert.equal(positions.get('a'), null)
  store.items.set(key, 'broken')
  assert.equal(positions.get('a'), null)
  for (let index = 0; index < 70; index++) positions.save(String(index), { kind: 'message', messageId: `message-${index}` })
  assert.equal(positions.get('0'), null)
  assert.deepEqual(positions.get('69'), { kind: 'message', messageId: 'message-69' })
})
