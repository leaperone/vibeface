import assert from 'node:assert/strict'
import test from 'node:test'
import { sha256Hex } from '../lib/byte-codec.ts'
import { canonicalSessionObjectBytes, envelopeDigest, isSameSessionObject, validateSessionChunkSet, validateSessionObject, verifySessionChunkSet } from './session.ts'
import type { SessionObjectChunk, SessionObjectEnvelope } from './types.ts'

const peer = '11'.repeat(32)
const payload = new TextEncoder().encode('encrypted bot session payload')
const base = async (patch: Partial<SessionObjectEnvelope> = {}): Promise<SessionObjectEnvelope> => ({
  version: 1, kind: 'message', object_id: 'object-1', conversation_id: 'conversation-1', sequence: 0, previous_object_hash: null,
  sender_peer: peer, receiver_peer: '22'.repeat(32), provider_peer: '33'.repeat(32), created_at_ms: 100, expires_at_ms: 200,
  payload_bytes: payload.byteLength, payload_hash: await sha256Hex(payload), chunk_count: 1, signature_digest: '44'.repeat(32), ...patch,
})

test('all supported object kinds share the same validated envelope', async () => {
  for (const kind of ['message', 'file', 'bot_config', 'session_state'] as const) assert.equal(validateSessionObject(await base({ kind })).kind, kind)
  for (const kind of ['secret', 'wallet_key', 'provider_credential']) {
    const invalid = await base({ kind: kind as never })
    assert.throws(() => validateSessionObject(invalid))
  }
})

test('canonical bytes and digest do not depend on object key insertion order', async () => {
  const object = await base()
  const { signature_digest, ...rest } = object
  const reordered = JSON.parse(JSON.stringify({ signature_digest, ...rest })) as SessionObjectEnvelope
  assert.deepEqual(canonicalSessionObjectBytes(object), canonicalSessionObjectBytes(reordered))
  assert.equal(await envelopeDigest(object), await envelopeDigest(reordered))
})

test('chunk validation rejects missing, duplicate, out of order, and tampered chunks', async () => {
  const object = await base({ payload_bytes: payload.byteLength, chunk_count: 2 })
  const split = Math.ceil(payload.byteLength / 2)
  const chunks: SessionObjectChunk[] = [
    { object_id: object.object_id, index: 0, offset: 0, bytes: split, payload: payload.slice(0, split) },
    { object_id: object.object_id, index: 1, offset: split, bytes: payload.byteLength - split, payload: payload.slice(split) },
  ]
  assert.deepEqual(validateSessionChunkSet(object, chunks), payload)
  assert.deepEqual(await verifySessionChunkSet(object, chunks), payload)
  assert.throws(() => validateSessionChunkSet(object, chunks.slice(0, 1)))
  assert.throws(() => validateSessionChunkSet(object, [chunks[0], chunks[0]]))
  assert.throws(() => validateSessionChunkSet(object, [chunks[1], chunks[0]]))
  await assert.rejects(verifySessionChunkSet(object, [{ ...chunks[0], payload: new Uint8Array(split) }, chunks[1]]))
})

test('object identity is stable across retries and structural validation rejects malformed metadata', async () => {
  const object = await base()
  assert.equal(isSameSessionObject(object, { ...object, signature_digest: '55'.repeat(32) }), true)
  assert.throws(() => validateSessionObject({ ...object, object_id: '' }))
  assert.throws(() => validateSessionObject({ ...object, sequence: 1 }))
  assert.throws(() => validateSessionObject({ ...object, provider_peer: 'not-a-peer' }))
  assert.throws(() => validateSessionObject({ ...object, payload_bytes: 0 }))
})
