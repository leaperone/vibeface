import type { TranscriptMessage } from '../chat/types.ts'
import { byRank } from '../chat/timeline.ts'
import { parseConversation } from './model.ts'

export const localRecordLimits = { bytes: 8 * 1024 * 1024, chats: 50, messagesPerChat: 500 } as const

export type LocalTranscript = { networkId: string; deviceId: string; conversationId: string; messages: TranscriptMessage[] }
export type LocalDraft = { networkId: string; deviceId: string; conversationId: string; text: string; attachments: { uri: string; name: string; size: number | null; mimeType: string | null }[] }

function localAttachment(value: unknown): LocalDraft['attachments'][number] | null {
  if (!value || typeof value !== 'object') return null
  const item = value as Record<string, unknown>
  if (typeof item.uri !== 'string' || item.uri.length > 4096 || !/^(file|content|ph|assets-library):\/\//.test(item.uri)
    || typeof item.name !== 'string' || item.name.length > 1024
    || !(item.size === null || typeof item.size === 'number' && Number.isFinite(item.size) && item.size >= 0)
    || !(item.mimeType === null || typeof item.mimeType === 'string' && item.mimeType.length <= 256)) return null
  return { uri: item.uri, name: item.name, size: item.size, mimeType: item.mimeType }
}

export function parseLocalDraft(raw: string | null, identity: Pick<LocalDraft, 'networkId' | 'deviceId' | 'conversationId'>): LocalDraft | null {
  if (raw === null || new TextEncoder().encode(raw).length > 128 * 1024) return null
  try {
    const value: unknown = JSON.parse(raw)
    if (!value || typeof value !== 'object') return null
    const item = value as Record<string, unknown>
    if (item.networkId !== identity.networkId || item.deviceId !== identity.deviceId || item.conversationId !== identity.conversationId
      || typeof item.text !== 'string' || !Array.isArray(item.attachments) || item.attachments.length > 20) return null
    const attachments = item.attachments.map(localAttachment)
    if (attachments.some((file) => file === null)) return null
    return { ...identity, text: item.text, attachments: attachments.filter((file): file is LocalDraft['attachments'][number] => file !== null) }
  } catch { return null }
}

export function encodeLocalDraft(draft: LocalDraft): string | null {
  if (draft.attachments.length > 20 || draft.attachments.some((file) => localAttachment(file) === null)) return null
  const raw = JSON.stringify(draft)
  return new TextEncoder().encode(raw).length <= 128 * 1024 ? raw : null
}

export function parseLocalTranscripts(raw: string | null, networkId: string, deviceId: string): LocalTranscript[] {
  if (raw === null || new TextEncoder().encode(raw).length > localRecordLimits.bytes) return []
  try {
    const value: unknown = JSON.parse(raw)
    if (!Array.isArray(value)) return []
    return boundLocalRecords(value.map((record) => {
      if (!record || typeof record !== 'object') return null
      const item = record as Record<string, unknown>
      if (typeof item.conversationId !== 'string') return null
      return parseLocalTranscript(record, { networkId, deviceId, conversationId: item.conversationId })
    }).filter((record): record is LocalTranscript => record !== null))
  } catch { return [] }
}

/** Parse an untrusted local snapshot and enforce its identity and size limits. */
export function parseLocalTranscript(value: unknown, identity: Pick<LocalTranscript, 'networkId' | 'deviceId' | 'conversationId'>): LocalTranscript | null {
  if (!value || typeof value !== 'object') return null
  const record = value as Record<string, unknown>
  if (record.networkId !== identity.networkId || record.deviceId !== identity.deviceId || record.conversationId !== identity.conversationId
    || !Array.isArray(record.messages) || record.messages.length > localRecordLimits.messagesPerChat
    || new TextEncoder().encode(JSON.stringify(value)).length > localRecordLimits.bytes) return null
  try {
    const messages = parseConversation({ conversation: null, messages: record.messages, works: [], deliveries: [], scheduler: null, checks: [], local_node: '',
      changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, questions: [], sessions: [], sent: [] }).messages
    return { ...identity, messages: byRank(messages) }
  } catch { return null }
}

/** Message IDs are stable; later copies replace earlier ones without allowing a partial snapshot to erase history. */
export function mergeTranscriptMessages(previous: readonly TranscriptMessage[], incoming: readonly TranscriptMessage[]): TranscriptMessage[] {
  const byId = new Map(previous.map((message) => [message.message_id, message]))
  for (const message of incoming) byId.set(message.message_id, message)
  return byRank([...byId.values()]).slice(-localRecordLimits.messagesPerChat)
}

/** Keep the newest bounded set of encoded records; corrupt entries are ignored. */
export function boundLocalRecords(records: readonly LocalTranscript[]): LocalTranscript[] {
  const bounded = records.slice(-localRecordLimits.chats).map((record) => ({ ...record, messages: record.messages.slice(-localRecordLimits.messagesPerChat) }))
  while (bounded.length && new TextEncoder().encode(JSON.stringify(bounded)).length > localRecordLimits.bytes) {
    const largest = bounded.reduce((best, item, index) => item.messages.length > bounded[best].messages.length ? index : best, 0)
    if (bounded[largest].messages.length) bounded[largest] = { ...bounded[largest], messages: bounded[largest].messages.slice(Math.max(1, Math.floor(bounded[largest].messages.length / 4))) }
    else bounded.splice(largest, 1)
  }
  return bounded
}

export function upsertLocalTranscript(records: readonly LocalTranscript[], incoming: LocalTranscript): LocalTranscript[] {
  const current = records.find((item) => item.networkId === incoming.networkId && item.deviceId === incoming.deviceId && item.conversationId === incoming.conversationId)
  return boundLocalRecords([...records.filter((item) => item !== current), { ...incoming, messages: mergeTranscriptMessages(current?.messages ?? [], incoming.messages) }])
}
