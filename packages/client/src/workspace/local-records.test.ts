import test from 'node:test'
import assert from 'node:assert/strict'
import { encodeLocalDraft, localRecordLimits, mergeTranscriptMessages, parseLocalDraft, parseLocalTranscript, parseLocalTranscripts, upsertLocalTranscript } from './local-records.ts'

const identity = { networkId: 'n1', deviceId: 'd1', conversationId: 'c1' }
const message = { message_id: 'm1', event_hash: 'e1', role: 'user', text: 'hello', created_at_ms: 1 }

test('local transcript validates identity and keeps stable messages across partial merges', () => {
  const parsed = parseLocalTranscript({ ...identity, messages: [message] }, identity)
  assert.deepEqual(parsed?.messages.map((item) => item.message_id), ['m1'])
  assert.equal(parseLocalTranscript({ ...identity, messages: [message] }, { ...identity, networkId: 'n2' }), null)
  assert.deepEqual(mergeTranscriptMessages([message], []).map((item) => item.message_id), ['m1'])
})

test('corrupt and oversized local transcript records are discarded', () => {
  assert.equal(parseLocalTranscript({ ...identity, messages: [{ ...message, created_at_ms: 'bad' }] }, identity), null)
  assert.equal(parseLocalTranscript({ ...identity, messages: Array.from({ length: 501 }, (_, index) => ({ ...message, message_id: `${index}` })) }, identity), null)
})

test('stored transcripts round trip through a persistent string store and reject malformed optional fields', () => {
  const store = new Map<string, string>()
  const first = { ...message, rank: '0002', created_at_ms: 1 }
  const later = { ...message, message_id: 'm2', event_hash: 'e2', rank: '0001', created_at_ms: 100 }
  const updated = upsertLocalTranscript([], { ...identity, messages: [first] })
  const withSecond = upsertLocalTranscript(updated, { ...identity, messages: [later] })
  store.set('records', JSON.stringify(withSecond))
  assert.deepEqual(parseLocalTranscripts(store.get('records') ?? null, identity.networkId, identity.deviceId)[0]?.messages.map((item) => item.message_id), ['m2', 'm1'])
  assert.deepEqual(parseLocalTranscripts(store.get('records') ?? null, 'different-network', identity.deviceId), [])
  assert.deepEqual(parseLocalTranscripts(store.get('records') ?? null, identity.networkId, 'different-device'), [])
  assert.deepEqual(parseLocalTranscripts('{broken', identity.networkId, identity.deviceId), [])
  assert.equal(parseLocalTranscript({ ...identity, messages: [{ ...message, sender: 'bad' }] }, identity), null)
  assert.equal(parseLocalTranscript({ ...identity, messages: [{ ...message, status: 'unknown' }] }, identity), null)
})

test('transcript cache enforces encoded size and chat-count bounds', () => {
  const one = { ...identity, messages: [message] }
  const many = Array.from({ length: 51 }, (_, index) => ({ ...one, conversationId: `c${index}` }))
  const capped = upsertLocalTranscript(many.slice(0, 50), { ...one, conversationId: 'last' })
  assert.equal(capped.length, 50)
  assert.deepEqual(parseLocalTranscripts(' '.repeat(8 * 1024 * 1024 + 1), identity.networkId, identity.deviceId), [])
  const oversized = upsertLocalTranscript([], { ...one, messages: [{ ...message, text: 'x'.repeat(localRecordLimits.bytes + 1) }] })
  assert.ok(new TextEncoder().encode(JSON.stringify(oversized)).length <= localRecordLimits.bytes)
})

test('draft text and attachment metadata round trip only for the exact network, device, and chat', () => {
  const draft = { ...identity, text: 'keep this', attachments: [{ uri: 'file:///cache/a', name: 'a.png', size: 12, mimeType: 'image/png' }] }
  const raw = encodeLocalDraft(draft)
  assert.deepEqual(parseLocalDraft(raw, identity), draft)
  assert.equal(parseLocalDraft(raw, { ...identity, deviceId: 'other' }), null)
  assert.equal(parseLocalDraft(raw, { ...identity, networkId: 'other' }), null)
  assert.equal(parseLocalDraft(raw, { ...identity, conversationId: 'other' }), null)
})
