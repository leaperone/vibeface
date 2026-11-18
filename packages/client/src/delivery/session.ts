import { sha256Hex } from '../lib/byte-codec.ts'
import { SESSION_OBJECT_KINDS, type SessionObjectChunk, type SessionObjectEnvelope, type SessionObjectKind } from './types.ts'

export const SESSION_CHUNK_BYTES = 65_536
export const SESSION_MAX_CHUNKS = 1_024
export const SESSION_MAX_PAYLOAD_BYTES = 64 * 1024 * 1024

const hashPattern = /^[a-f0-9]{64}$/
const peerPattern = hashPattern
const idPattern = /^[^\u0000-\u001f\u007f]{1,128}$/u
const encoder = new TextEncoder()

function fail(message: string): never { throw new Error(message) }
function field(value: string): Uint8Array {
  const bytes = encoder.encode(value)
  const length = new Uint8Array(4)
  new DataView(length.buffer).setUint32(0, bytes.length)
  return new Uint8Array([...length, ...bytes])
}
function id(value: unknown, label: string): string {
  if (typeof value !== 'string' || !idPattern.test(value)) fail(`${label} 无效。`)
  return value
}
function peer(value: unknown, label: string): string {
  if (typeof value !== 'string' || !peerPattern.test(value)) fail(`${label} 无效。`)
  return value
}
function hash(value: unknown, label: string, nullable = false): string | null {
  if (nullable && value === null) return null
  if (typeof value !== 'string' || !hashPattern.test(value)) fail(`${label} 无效。`)
  return value
}
function integer(value: unknown, label: string, max: number): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0 || value > max) fail(`${label} 无效。`)
  return value
}
function kind(value: unknown): SessionObjectKind {
  if (typeof value !== 'string' || !(SESSION_OBJECT_KINDS as readonly string[]).includes(value)) fail('会话对象类型不受支持。')
  return value as SessionObjectKind
}

export function validateSessionObject(value: SessionObjectEnvelope): SessionObjectEnvelope {
  const allowed = new Set(['version', 'kind', 'object_id', 'conversation_id', 'sequence', 'previous_object_hash', 'sender_peer', 'receiver_peer', 'provider_peer', 'created_at_ms', 'expires_at_ms', 'payload_bytes', 'payload_hash', 'chunk_count', 'signature_digest'])
  if (Object.keys(value as object).some(key => !allowed.has(key))) fail('会话对象包含未声明字段。')
  if (value.version !== 1) fail('会话对象版本不受支持。')
  const result: SessionObjectEnvelope = {
    version: 1,
    kind: kind(value.kind),
    object_id: id(value.object_id, '对象编号'),
    conversation_id: id(value.conversation_id, '会话编号'),
    sequence: integer(value.sequence, '对象序号', Number.MAX_SAFE_INTEGER),
    previous_object_hash: hash(value.previous_object_hash, '前序对象摘要', true),
    sender_peer: peer(value.sender_peer, '发送设备'),
    receiver_peer: peer(value.receiver_peer, '接收设备'),
    provider_peer: peer(value.provider_peer, '中继设备'),
    created_at_ms: integer(value.created_at_ms, '创建时间', Number.MAX_SAFE_INTEGER),
    expires_at_ms: integer(value.expires_at_ms, '过期时间', Number.MAX_SAFE_INTEGER),
    payload_bytes: integer(value.payload_bytes, '载荷大小', SESSION_MAX_PAYLOAD_BYTES),
    payload_hash: hash(value.payload_hash, '载荷摘要')!,
    chunk_count: integer(value.chunk_count, '分片数量', SESSION_MAX_CHUNKS),
    signature_digest: hash(value.signature_digest, '签名摘要')!,
  }
  if (result.expires_at_ms <= result.created_at_ms) fail('会话对象过期时间必须晚于创建时间。')
  if (result.payload_bytes === 0 || result.chunk_count === 0) fail('会话对象不能没有载荷。')
  if (result.chunk_count > result.payload_bytes) fail('会话对象分片数量过多。')
  if (result.sequence === 0 && result.previous_object_hash !== null) fail('首个会话对象不能带前序摘要。')
  if (result.sequence > 0 && result.previous_object_hash === null) fail('后续会话对象必须带前序摘要。')
  return result
}

/** Fixed-width length-prefixed fields keep signatures independent of JSON key order. */
export function canonicalSessionObjectBytes(value: SessionObjectEnvelope): Uint8Array {
  const object = validateSessionObject(value)
  const fields = [
    'v1', object.kind, object.object_id, object.conversation_id, String(object.sequence), object.previous_object_hash ?? '',
    object.sender_peer, object.receiver_peer, object.provider_peer, String(object.created_at_ms), String(object.expires_at_ms),
    String(object.payload_bytes), object.payload_hash, String(object.chunk_count), object.signature_digest,
  ].map(field)
  const output = new Uint8Array(fields.reduce((total, current) => total + current.length, 0))
  let offset = 0
  for (const current of fields) { output.set(current, offset); offset += current.length }
  return output
}

export function isSameSessionObject(left: SessionObjectEnvelope, right: SessionObjectEnvelope): boolean {
  return left.object_id === right.object_id
}

export async function envelopeDigest(value: SessionObjectEnvelope): Promise<string> {
  return sha256Hex(canonicalSessionObjectBytes(value))
}

export function validateSessionChunkSet(envelope: SessionObjectEnvelope, chunks: readonly SessionObjectChunk[]): Uint8Array {
  const object = validateSessionObject(envelope)
  if (chunks.length !== object.chunk_count) fail('会话对象分片数量不匹配。')
  const seen = new Set<number>()
  const output = new Uint8Array(object.payload_bytes)
  let expectedOffset = 0
  for (const chunk of chunks) {
    if (chunk.object_id !== object.object_id || seen.has(chunk.index)) fail('会话对象包含重复或错误分片。')
    if (chunk.index !== seen.size || chunk.offset !== expectedOffset || chunk.bytes !== chunk.payload.byteLength) fail('会话对象分片顺序或大小无效。')
    if (chunk.bytes <= 0 || chunk.bytes > SESSION_CHUNK_BYTES || expectedOffset + chunk.bytes > object.payload_bytes) fail('会话对象分片越界。')
    seen.add(chunk.index)
    output.set(chunk.payload, chunk.offset)
    expectedOffset += chunk.bytes
  }
  if (expectedOffset !== object.payload_bytes) fail('会话对象缺少载荷分片。')
  return output
}

export async function verifySessionChunkSet(envelope: SessionObjectEnvelope, chunks: readonly SessionObjectChunk[]): Promise<Uint8Array> {
  const payload = validateSessionChunkSet(envelope, chunks)
  if (await sha256Hex(payload) !== envelope.payload_hash) fail('会话对象载荷摘要不匹配。')
  return payload
}
