import type {
  ConversationPayload,
  MessageRequest,
  MissingContext,
  PendingMessage,
  TranscriptMessage,
} from './types.ts'

export type NetworkCarrier = {
  network_id?: unknown
  account?: unknown
}

export type SendGate = {
  hasConversation: boolean
  loaded: boolean
  missing: boolean
  schedulerOnline: boolean
  managerReady: boolean
  schedulerNodeId: string | null
  busy: boolean
}

const PENDING_KEY = 'vibeface.pending-messages'
const FRONTIER_KEY = 'vibeface.frontiers'

export function networkScope(session: NetworkCarrier | null): string {
  if (!session || typeof session !== 'object') return 'anonymous'
  if (typeof session.network_id === 'string' && session.network_id) return session.network_id
  if (session.account && typeof session.account === 'object' && 'network_id' in session.account) {
    const networkId = session.account.network_id
    if (typeof networkId === 'string' && networkId) return networkId
  }
  return 'anonymous'
}

export function scopedKey(session: NetworkCarrier | null, key: string): string {
  return `${key}.${networkScope(session)}`
}

export function roleLabel(role: string): string {
  if (role === 'user') return '你'
  if (role === 'assistant') return '调度 Agent'
  return role
}

export function countMissing(value: unknown): number {
  if (Array.isArray(value)) return value.length
  if (typeof value === 'number' && Number.isFinite(value)) return value
  return 0
}

export function isIncomplete(missing: MissingContext | null | undefined): boolean {
  if (!missing) return false
  return (['events', 'blobs', 'conflicts'] as const).some((key) => countMissing(missing[key]) > 0)
}

export function canSend(input: SendGate): boolean {
  return Boolean(
    input.hasConversation &&
      input.loaded &&
      !input.missing &&
      input.schedulerOnline &&
      input.managerReady &&
      input.schedulerNodeId &&
      !input.busy,
  )
}

export function composerHint(input: Omit<SendGate, 'busy'>): string {
  if (!input.hasConversation || !input.loaded) return '先创建或选择一个会话。'
  if (input.missing) return '等待上下文补齐后再发送。'
  if (!input.managerReady || !input.schedulerNodeId) {
    return '设备尚未配置调度 Agent。执行会话仍可直接操作。'
  }
  if (!input.schedulerOnline) return '所选调度设备离线，请切换其他入口或设备。'
  return '消息将交给所选设备上的调度 Agent。'
}

export function newId(): string {
  return globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`
}

export function errorStatus(error: unknown): number | undefined {
  if (typeof error === 'object' && error && 'status' in error && typeof (error as { status: unknown }).status === 'number') {
    return (error as { status: number }).status
  }
  return undefined
}

export function errorText(error: unknown): string {
  if (error instanceof Error && error.message) return error.message
  return '请求失败。'
}

export function pendingAfterFailure(status: number | undefined): 'keep' | 'drop' {
  if (status !== undefined && status >= 400 && status < 500 && status !== 409) return 'drop'
  return 'keep'
}

export function prepareSend(input: {
  conversationId: string
  text: string
  schedulerNodeId: string
  pending: PendingMessage | null
  heads: string[]
}): { ok: true; pending: PendingMessage; body: MessageRequest } | { ok: false; error: string } {
  const text = input.text.trim()
  if (!text) return { ok: false, error: '先填写消息内容。' }
  if (input.pending?.text && input.pending.text !== text) {
    return { ok: false, error: '此会话有一条消息尚未确认，请先刷新核对原消息，再发送新内容。' }
  }
  const pending: PendingMessage = {
    message_id: input.pending?.message_id || newId(),
    conversation_id: input.conversationId,
    text,
    scheduler_node: input.pending?.scheduler_node || input.schedulerNodeId,
  }
  return {
    ok: true,
    pending,
    body: {
      conversation_id: pending.conversation_id,
      message_id: pending.message_id,
      text: pending.text,
      basis_heads: input.heads,
      scheduler_node: pending.scheduler_node || input.schedulerNodeId,
    },
  }
}

export function messageKey(message: TranscriptMessage): string {
  return message.event_hash || message.message_id
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null
  return value as Record<string, unknown>
}

function asString(value: unknown): string | null {
  return typeof value === 'string' ? value : null
}

export function parseConversation(value: unknown): ConversationPayload {
  const record = asRecord(value)
  const messages = Array.isArray(record?.messages)
    ? record.messages.flatMap((item) => {
        const message = asRecord(item)
        const messageId = asString(message?.message_id)
        const role = asString(message?.role)
        const text = asString(message?.text)
        if (!message || !messageId || !role || text === null) return []
        const parsed: TranscriptMessage = { message_id: messageId, role, text }
        const hash = asString(message.event_hash)
        if (hash) parsed.event_hash = hash
        if (typeof message.created_at_ms === 'number') parsed.created_at_ms = message.created_at_ms
        return [parsed]
      })
    : []
  const heads = Array.isArray(record?.heads) ? record.heads.filter((item): item is string => typeof item === 'string') : []
  const missing = asRecord(record?.missing) ?? {}
  return { messages, heads, missing }
}

type KeyValueStore = {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
}

export function readPending(store: KeyValueStore, session: NetworkCarrier | null): Map<string, PendingMessage> {
  try {
    const parsed = JSON.parse(store.getItem(scopedKey(session, PENDING_KEY)) ?? '[]') as unknown
    if (!Array.isArray(parsed)) return new Map()
    const entries = parsed.flatMap((entry) => {
      if (!Array.isArray(entry) || entry.length !== 2 || typeof entry[0] !== 'string') return []
      const message = asRecord(entry[1])
      const messageId = asString(message?.message_id)
      const text = asString(message?.text)
      if (!message || !messageId || text === null) return []
      const pending: PendingMessage = {
        message_id: messageId,
        conversation_id: asString(message.conversation_id) || entry[0],
        text,
      }
      const scheduler = asString(message.scheduler_node)
      if (scheduler) pending.scheduler_node = scheduler
      return [[entry[0], pending] as const]
    })
    return new Map(entries)
  } catch {
    return new Map()
  }
}

export function writePending(store: KeyValueStore, session: NetworkCarrier | null, pending: Map<string, PendingMessage>) {
  store.setItem(scopedKey(session, PENDING_KEY), JSON.stringify([...pending]))
}

export function readFrontier(store: KeyValueStore, session: NetworkCarrier | null, conversationId: string): string[] {
  try {
    const parsed = JSON.parse(store.getItem(scopedKey(session, FRONTIER_KEY)) ?? '{}') as unknown
    const record = asRecord(parsed)
    const heads = record?.[conversationId]
    return Array.isArray(heads) ? heads.filter((item): item is string => typeof item === 'string') : []
  } catch {
    return []
  }
}

export function writeFrontier(store: KeyValueStore, session: NetworkCarrier | null, conversationId: string, heads: string[]) {
  const current = (() => {
    try {
      return asRecord(JSON.parse(store.getItem(scopedKey(session, FRONTIER_KEY)) ?? '{}')) ?? {}
    } catch {
      return {}
    }
  })()
  store.setItem(scopedKey(session, FRONTIER_KEY), JSON.stringify({ ...current, [conversationId]: heads }))
}

export function browserStore(): KeyValueStore | null {
  try {
    if (typeof localStorage === 'undefined') return null
    return localStorage
  } catch {
    return null
  }
}
