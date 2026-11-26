import { isRecord } from '../lib/node.ts'
import { parseConversation, record } from './model.ts'

type SnapshotReceipt = { fields: Record<string, string>; signature: string; messages?: string[] }
export type SnapshotCondition = { if_snapshot: string; snapshot_receipt: SnapshotReceipt }
export type ValidatedSnapshot = { scope: string; raw: Record<string, unknown>; condition: SnapshotCondition }
const isHash = (value: unknown): value is string => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)

function condition(response: Record<string, unknown>): SnapshotCondition | null {
  const receipt = response.snapshot_receipt
  if (!isHash(response.snapshot_id) || !isRecord(receipt) || !isRecord(receipt.fields)
    || typeof receipt.signature !== 'string' || !/^[a-f0-9]{128}$/.test(receipt.signature)) return null
  const entries = Object.entries(receipt.fields)
  if (!entries.length || entries.length > 64 || !entries.every((entry): entry is [string, string] => isHash(entry[1]))) return null
  const messages = receipt.messages
  if (messages !== undefined && (!Array.isArray(messages) || messages.length > 128 || !messages.every(isHash))) return null
  return { if_snapshot: response.snapshot_id, snapshot_receipt: {
    fields: Object.fromEntries(entries), signature: receipt.signature, ...(messages === undefined ? {} : { messages }),
  } }
}

function messageRows(delta: unknown, previous: ValidatedSnapshot, next: SnapshotCondition): unknown[] | null {
  const before = previous.raw.messages, oldHashes = previous.condition.snapshot_receipt.messages, hashes = next.snapshot_receipt.messages
  if (!Array.isArray(before) || !oldHashes || before.length !== oldHashes.length || !hashes || !isRecord(delta)
    || !Array.isArray(delta.rows) || delta.rows.length !== hashes.length) return null
  const messages: unknown[] = []
  for (const [index, row] of delta.rows.entries()) {
    if (!isRecord(row) || Object.keys(row).length !== 1) return null
    if ('previous' in row) {
      const from = row.previous
      if (typeof from !== 'number' || !Number.isSafeInteger(from) || from < 0 || from >= before.length || oldHashes[from] !== hashes[index]) return null
      messages.push(before[from])
    } else if (isRecord(row.value)) messages.push(row.value)
    else return null
  }
  return messages
}

/** Reconstruct the exact Node response. Display-only rows and older history pages never enter this base. */
function patched(response: Record<string, unknown>, previous: ValidatedSnapshot, next: SnapshotCondition): Record<string, unknown> | null {
  const { removed } = response
  if (response.base_snapshot_id !== previous.condition.if_snapshot || !isRecord(response.fields) || !Array.isArray(removed)) return null
  const fields = { ...response.fields }
  if (response.messages_delta !== undefined) {
    if (Object.hasOwn(fields, 'messages')) return null
    const messages = messageRows(response.messages_delta, previous, next)
    if (!messages) return null
    fields.messages = messages
  }
  const oldHashes = previous.condition.snapshot_receipt.fields, hashes = next.snapshot_receipt.fields
  const missing = Object.keys(oldHashes).filter(key => !Object.hasOwn(hashes, key))
  if (removed.length !== missing.length || new Set(removed).size !== missing.length || !missing.every(key => removed.includes(key))) return null
  if (Object.keys(fields).some(key => !Object.hasOwn(hashes, key) || hashes[key] === oldHashes[key])) return null
  const raw = { ...previous.raw, ...fields }
  for (const key of missing) delete raw[key]
  for (const [key, hash] of Object.entries(hashes)) {
    if (oldHashes[key] !== hash && !Object.hasOwn(fields, key)) return null
  }
  return raw
}

export async function readValidatedSnapshot(input: {
  scope: string
  conversationId: string
  snapshot: { current: ValidatedSnapshot | null }
  current(): boolean
  read(previous?: SnapshotCondition): Promise<unknown>
}) {
  const previous = input.snapshot.current?.scope === input.scope ? input.snapshot.current : null
  let response = record(await input.read(previous?.condition))
  if (!input.current()) return null
  if (response.unchanged === true && previous && previous.condition.if_snapshot === response.snapshot_id) {
    return parseConversation(previous.raw, input.conversationId)
  }
  if (response.snapshot_delta === true && previous) {
    const next = condition(response)
    const raw = next && patched(response, previous, next)
    if (raw && next) {
      try {
        const payload = parseConversation(raw, input.conversationId)
        input.snapshot.current = { scope: input.scope, raw, condition: next }
        return payload
      } catch { /* The full read below replaces an unusable delta. */ }
    }
  }
  if (response.unchanged === true || response.snapshot_delta === true) {
    input.snapshot.current = null
    response = record(await input.read())
    if (!input.current()) return null
    if (response.unchanged === true || response.snapshot_delta === true) throw new Error('设备没有返回完整的对话。')
  }
  const { snapshot_id, snapshot_receipt, ...raw } = response
  const payload = parseConversation(raw, input.conversationId)
  const next = condition({ snapshot_id, snapshot_receipt })
  const keys = Object.keys(raw)
  input.snapshot.current = next && keys.length === Object.keys(next.snapshot_receipt.fields).length && keys.every(key => Object.hasOwn(next.snapshot_receipt.fields, key))
    ? { scope: input.scope, raw, condition: next } : null
  return payload
}
