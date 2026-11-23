import { pageIds } from './web-log.ts'

/**
 * A short number a person can quote for one failed request. The log record of that failure carries it as
 * `fields.ref`, so a bot reading the logs finds the request the person saw fail.
 */
export type FailureRef = { ref: string; at_ms: number; message: string; conversation_id?: string }

const KEPT = 20
/** A shown message names its failure for this long; after that the same words are a new failure without a number. */
const MATCH_MS = 10 * 60_000
const byCause = new WeakMap<object, FailureRef>()
let recent: FailureRef[] = []
const listeners = new Set<() => void>()

/** Eight base-36 characters. */
export function newRef(bytes: Uint8Array = globalThis.crypto.getRandomValues(new Uint8Array(8))): string {
  return Array.from(bytes.slice(0, 8), (byte) => (byte % 36).toString(36)).join('')
}

/** Notes a failure the page shows as `message`; `cause` is the thrown value the page keeps. */
export function recordFailure(cause: unknown, message: string, conversation_id?: string, now = Date.now()): FailureRef {
  const failure: FailureRef = { ref: newRef(), at_ms: now, message, ...(conversation_id ? { conversation_id } : {}) }
  if (cause !== null && typeof cause === 'object') byCause.set(cause, failure)
  recent = [...recent, failure].slice(-KEPT)
  for (const listener of listeners) listener()
  return failure
}

/** The failure an error display shows: by the thrown value, else by the words it shows. */
export function failureRef(error: unknown, now = Date.now()): FailureRef | undefined {
  if (error !== null && typeof error === 'object') return byCause.get(error)
  if (typeof error !== 'string' || !error) return undefined
  return [...recent].reverse().find((item) => item.message === error && now - item.at_ms <= MATCH_MS)
}

export function lastFailure(): FailureRef | undefined { return recent.at(-1) }
export function subscribeFailures(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener) } }

const stamp = (ms: number) => new Date(ms).toISOString()

/** What 复制 puts on the clipboard: the number, when it failed and its conversation. */
export function failureCopyText(failure: FailureRef): string {
  return [`错误编号 ${failure.ref}`, `时间 ${stamp(failure.at_ms)}`, failure.conversation_id ? `会话 ${failure.conversation_id}` : null].filter(Boolean).join('\n')
}

/** The message a problem report sends to the primary bot; parts the person left out are omitted. */
export function problemReport({ description, conversation_id, ref }: { description: string; conversation_id?: string; ref?: string }): string {
  const text = description.trim()
  if (!text) throw new Error('先写下遇到的问题。')
  return [`[问题报告] ${text}`, conversation_id ? `会话：${conversation_id}` : null, ref ? `错误编号：${ref}` : null].filter(Boolean).join('\n')
}

const conversationId = (value: unknown) => typeof value === 'string' && /^[A-Za-z0-9_.:-]{1,128}$/.test(value) ? value : undefined
/** The conversation a request names, directly or inside an `rpc` call's own params; else the one the page shows. */
export function requestConversation(params: Record<string, unknown>): string | undefined {
  const inner = params.params
  const page = (globalThis as { location?: { pathname: string; search: string } }).location
  return conversationId(params.conversation_id) ?? (inner && typeof inner === 'object' ? conversationId((inner as Record<string, unknown>).conversation_id) : undefined)
    ?? (page ? pageIds(page).conversation_id : undefined)
}
