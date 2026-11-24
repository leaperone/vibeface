/** Browser and phone records for the connected device's log: errors, failed requests and user actions. */
import type { KeyValue } from "./platform.ts"

export type LogLevel = "trace" | "debug" | "info" | "warn" | "error"
export type LogSource = "node" | "web" | "agent" | "cli" | "codex" | "desktop" | "mobile"

export type LogRecord = {
  ts_ms: number
  level: LogLevel
  source: LogSource
  event: string
  message: string
  conversation_id?: string
  work_id?: string
  thread_id?: string
  fields?: Record<string, unknown>
}

export type WebRecord = Pick<LogRecord, "level" | "event" | "message" | "conversation_id" | "work_id" | "fields">
/** Where records come from, as the `log` command's `source`. */
type ClientSource = Extract<LogSource, "web" | "mobile">
type Sink = (records: WebRecord[], source: ClientSource) => Promise<unknown>
/** One `sendLogs`: records the device accepted, records still kept, and whether a device was connected. */
export type SendResult = { sent: number; left: number; connected: boolean }

/** The device rejects a record over 8 KiB of JSON and a batch over 20. */
export const MAX_RECORD_BYTES = 7000
export const MAX_BATCH = 20
const MAX_QUEUED = 100

const bytes = (value: unknown) => new TextEncoder().encode(JSON.stringify(value)).length

/** Characters of a stack an oversized record keeps. */
const STACK_PREFIX = 2000

/** `JSON.stringify` writes a lone surrogate as an escape the device's JSON parser refuses. */
const LONE_ESCAPE = /\\ud[89a-f][0-9a-f]{2}/i
/** `text` with each lone surrogate as U+FFFD; pairs stay. Written out because `toWellFormed` is not on every phone. */
export const wellFormed = (text: string) =>
  text.replace(/[\uD800-\uDBFF][\uDC00-\uDFFF]|[\uD800-\uDFFF]/g, unit => unit.length === 2 ? unit : "\uFFFD")
/** `record` with every string well formed, the same object when it already is. */
const wellFormedRecord = (record: WebRecord): WebRecord => LONE_ESCAPE.test(JSON.stringify(record))
  ? JSON.parse(JSON.stringify(record), (_key, value: unknown) => typeof value === "string" ? wellFormed(value) : value) as WebRecord
  : record
/** The first `length` UTF-16 units of `text`, one fewer when the cut would split a surrogate pair. */
const cut = (text: string, length: number) =>
  text.slice(0, length > 0 && length < text.length && /[\uD800-\uDBFF]/.test(text[length - 1]!) ? length - 1 : length)

/**
 * Shrinks fields first, keeping `fatal` and the start of `stack`, then the longer of the stack and the message; rejects
 * records still over budget once both are exhausted.
 */
export function fit(input: WebRecord): WebRecord {
  const record = wellFormedRecord(input)
  if (bytes(record) <= MAX_RECORD_BYTES) return record
  const { fatal, stack } = record.fields ?? {}
  let kept = typeof stack === "string" ? cut(stack, STACK_PREFIX) : ""
  const fields = () => record.fields && {
    truncated: true, ...(fatal === undefined ? {} : { fatal }), ...(kept ? { stack: `${kept}…` } : {}),
  }
  let fitted: WebRecord = { ...record, fields: fields() }
  let message = record.message
  while (bytes(fitted) > MAX_RECORD_BYTES) {
    if (!kept && !message.length) throw new Error("Log record exceeds size limit.")
    if (kept.length > message.length) kept = cut(kept, Math.floor(kept.length * 0.8))
    else message = cut(message, Math.floor(message.length * 0.8))
    fitted = { ...record, message: message === record.message ? message : `${message}…`, fields: fields() }
  }
  return fitted
}

/**
 * What a failed request said before the page worded it for the reader: the device's error code and its own
 * text, or the transport's. `shown` is the worded message; the same text is not kept twice.
 */
export function failureFields(cause: unknown, shown: string): { code?: string; cause?: string } {
  const code = (cause as { code?: unknown } | null)?.code
  const raw = (cause instanceof Error ? cause.message : String(cause)).slice(0, 500)
  return { ...(typeof code === "string" ? { code } : {}), ...(raw && raw !== shown ? { cause: raw } : {}) }
}

/** A thrown value as a log message and its stack, for uncaught errors on every client. */
export function describeError(cause: unknown): { message: string; stack?: string } {
  if (cause instanceof Error) return { message: `${cause.name}: ${cause.message}`, stack: cause.stack }
  if (typeof cause === "string") return { message: cause }
  try { return { message: JSON.stringify(cause) ?? String(cause) } } catch { return { message: String(cause) } }
}

/** The conversation and task the page shows, from `?conversation=` and `/work/<id>`. */
export function pageIds(location: { pathname: string; search: string }): Pick<WebRecord, "conversation_id" | "work_id"> {
  const conversation = new URLSearchParams(location.search).get("conversation")
  const work = /\/work\/([^/]+)/.exec(location.pathname)?.[1]
  const valid = (id: string | null | undefined) => id && /^[A-Za-z0-9_.:-]{1,128}$/.test(id) ? id : undefined
  // A hand-edited address can carry a broken escape; it names no task.
  const decoded = (() => { try { return work && decodeURIComponent(work) } catch { return undefined } })()
  return { conversation_id: valid(conversation), work_id: valid(decoded) }
}

/** Sends as soon as a device is connected; records that arrive during a send go in the next batch. */
export function createReporter(): Reporter {
  const queue: WebRecord[] = []
  let sink: Sink | null = null
  let sending = false
  let pausedUntil = 0
  const schedule = () => { if (sink && queue.length && !sending && Date.now() >= pausedUntil) void Promise.resolve().then(flush) }
  async function flush() {
    if (!sink || sending || !queue.length) return
    sending = true
    const batch = queue.splice(0, MAX_BATCH)
    try { await sink(batch, "web") } catch (cause) {
      // Only a batch the device refused as invalid is dropped; any other failure keeps it for a later try.
      // A failed send is never reported as a record of its own, which would loop.
      const { responded, code } = (cause ?? {}) as { responded?: unknown; code?: unknown }
      if (!(responded === true && code === "log_rejected")) {
        queue.unshift(...batch)
        if (queue.length > MAX_QUEUED) queue.splice(0, queue.length - MAX_QUEUED)
        const wait = responded === true && code === "log_rate_limited" ? 60_000 : 5_000
        // The kept batch goes again with the next record after the pause, on the next connection or on a send.
        pausedUntil = Date.now() + wait
      }
    } finally { sending = false }
    schedule()
  }
  return {
    source: "web",
    report(record: WebRecord) {
      // Malformed records are dropped without interrupting the action being reported.
      try { queue.push(fit(record)) } catch { return }
      if (queue.length > MAX_QUEUED) queue.splice(0, queue.length - MAX_QUEUED)
      schedule()
    },
    /** Records wait in memory until a device connects; returns the detach function. */
    connect(next: Sink) {
      sink = next
      pausedUntil = 0
      schedule()
      return () => { if (sink === next) sink = null }
    },
    flush,
    async send() {
      const before = queue.length
      for (let left = Infinity; sink && queue.length && queue.length < left;) {
        left = queue.length
        await flush()
      }
      return { sent: before - queue.length, left: queue.length, connected: sink !== null }
    },
    pending: () => queue.length,
    clear() { queue.length = 0 },
    /** Records here live in memory only. */
    save() {},
  }
}

type Reporter = {
  source: ClientSource
  report(record: WebRecord): void
  connect(next: Sink): () => void
  flush(): Promise<unknown>
  send(): Promise<SendResult>
  pending(): number
  clear(): void
  /** Writes what a later save would, at once. */
  save(): void
}
type Kept = WebRecord & { fields: Record<string, unknown> & { client_seq: number; client_ts_ms: number } }
/** `sent_max` is the highest number ever handed to the device; a skipped record above it was never sent. */
type Buffer = { next: number; acked: number; sent_max: number; records: Kept[] }

const BUFFER_KEY = "vibeface.log-buffer"
const BUFFERED = 300
/** The device's budget for each sender. */
const PER_MINUTE = 120
const RETRY_MS = 15_000
/** Writes to storage wait this long so a burst is saved once; a fatal error is saved at once. */
const SAVE_MS = 500

/**
 * The phone's reporter: records survive restarts and crashes in `storage`, the newest 300 kept, each numbered with
 * `client_seq` and stamped with `client_ts_ms`. A batch leaves the buffer once the device accepts it or refuses it as
 * invalid (`log_rejected`); any other answer or an unknown result keeps it, and the device skips numbers it already
 * wrote, so sending it again is harmless. When the device skips records it names how many (`skipped`, the batch's
 * first ones) and its last written number (`max_seq`). Skipped records numbered at most `sent_max` were resends; a
 * skipped record above it, or a `max_seq` at or above the next number here, means the numbering went back: the records
 * the device wrote leave the buffer, and the rest get new numbers above `max_seq` and go again.
 */
export function createBufferedReporter(storage: KeyValue, { now = Date.now, later = (run: () => void, ms: number) => { setTimeout(run, ms) } } = {}): Reporter {
  const read = (): Buffer => {
    try {
      const saved = JSON.parse(storage.getItem(BUFFER_KEY) ?? "null") as Partial<Buffer> | null
      if (saved && Number.isSafeInteger(saved.next) && Number.isSafeInteger(saved.acked) && Number.isSafeInteger(saved.sent_max)
        && Array.isArray(saved.records)) {
        const acked = saved.acked as number
        return { next: saved.next as number, acked, sent_max: saved.sent_max as number,
          records: saved.records.filter(item => Number(item?.fields?.client_seq) > acked) }
      }
    } catch { /* a damaged buffer starts over */ }
    // Numbering from the clock keeps it above what the device saw before a reinstall.
    return { next: now(), acked: 0, sent_max: 0, records: [] }
  }
  const buffer = read()
  const save = () => { try { storage.setItem(BUFFER_KEY, JSON.stringify(buffer)) } catch { /* memory still holds them */ } }
  let saving = false
  const saveSoon = () => {
    if (saving) return
    saving = true
    later(() => { saving = false; save() }, SAVE_MS)
  }
  const spent: { at: number; count: number }[] = []
  let sink: Sink | null = null
  let sending: Promise<SendResult> | null = null
  let limitedUntil = 0
  let retryAt = 0
  let waiting = false
  const room = () => {
    while (spent[0] && now() - spent[0].at >= 60_000) spent.shift()
    return PER_MINUTE - spent.reduce((sum, item) => sum + item.count, 0)
  }
  const wake = (ms: number) => {
    if (waiting) return
    waiting = true
    later(() => { waiting = false; schedule() }, Math.max(ms, 0))
  }
  const schedule = () => { if (sink && buffer.records.length && !sending) void send() }
  const ack = (seq: number) => {
    buffer.acked = Math.max(buffer.acked, seq)
    buffer.records = buffer.records.filter(item => item.fields.client_seq > buffer.acked)
    saveSoon()
  }
  /** Numbers every kept record again, from just above what the device has written; none of the new numbers was sent. */
  const renumber = (written: number) => {
    buffer.next = written + 1
    buffer.acked = buffer.sent_max = written
    buffer.records = buffer.records.map(item => ({ ...item, fields: { ...item.fields, client_seq: buffer.next++ } }))
    // At once: the new numbers must not go out before the device's numbering is saved.
    save()
  }
  async function drain(): Promise<SendResult> {
    let sent = 0
    while (sink && buffer.records.length) {
      const pause = Math.max(limitedUntil, retryAt) - now()
      const free = room()
      if (pause > 0 || free <= 0) { wake(pause > 0 ? pause : 60_000 - (now() - spent[0]!.at)); break }
      const batch = buffer.records.slice(0, Math.min(MAX_BATCH, free))
      const last = batch[batch.length - 1]!.fields.client_seq
      const sentBefore = buffer.sent_max
      // Saved before it goes: a resend after a crash must not look like numbering that went back.
      if (last > sentBefore) { buffer.sent_max = last; save() }
      spent.push({ at: now(), count: batch.length })
      try {
        const answer = await sink(batch, "mobile") as { skipped?: unknown; max_seq?: unknown } | null
        const skipped = Number.isSafeInteger(answer?.skipped) ? Math.min(answer!.skipped as number, batch.length) : 0
        const written = answer?.max_seq
        const back = skipped > 0 && Number.isSafeInteger(written)
          && ((written as number) >= buffer.next || batch.slice(0, skipped).some(item => item.fields.client_seq > sentBefore))
        if (back) {
          const accepted = new Set(batch.slice(skipped))
          buffer.records = buffer.records.filter(item => !accepted.has(item))
          renumber(written as number)
          sent += accepted.size
        } else {
          // Skipped records were sent before and written then.
          ack(last)
          sent += batch.length
        }
      } catch (cause) {
        const { responded, code } = (cause ?? {}) as { responded?: unknown; code?: unknown }
        // The device answered that it wrote nothing, so this batch was not handed over; only an unknown result counts.
        if (responded === true && buffer.sent_max !== sentBefore) { buffer.sent_max = sentBefore; saveSoon() }
        if (responded === true && code === "log_rate_limited") limitedUntil = now() + 60_000
        // The device refused the batch itself; sending it again cannot change that.
        else if (responded === true && code === "log_rejected") ack(last)
        // Maintenance, a lost permission or any other answer may pass; the batch waits like an unknown result.
        else retryAt = now() + RETRY_MS
      }
    }
    return { sent, left: buffer.records.length, connected: sink !== null }
  }
  function send() {
    if (!sending) sending = drain().finally(() => { sending = null })
    return sending
  }
  return {
    source: "mobile",
    report(record: WebRecord) {
      let kept: Kept
      try {
        const fitted = fit(record)
        kept = { ...fitted, fields: { ...fitted.fields, client_seq: buffer.next, client_ts_ms: now() } }
      } catch { return }
      buffer.next++
      buffer.records.push(kept)
      if (buffer.records.length > BUFFERED) buffer.records.splice(0, buffer.records.length - BUFFERED)
      // A fatal error may end the app before a later save runs.
      if (record.fields?.fatal === true) save()
      else saveSoon()
      schedule()
    },
    connect(next: Sink) {
      sink = next
      schedule()
      return () => { if (sink === next) sink = null }
    },
    flush: send,
    /** Sends now instead of waiting out an earlier unknown result; the device's budget still applies. */
    send() { retryAt = 0; return send() },
    pending: () => buffer.records.length,
    clear() { buffer.records = []; buffer.acked = buffer.next - 1; save() },
    save,
  }
}

let reporter: Reporter = createReporter()

/** The phone keeps its records in `storage` until its device accepts them; call once, before anything reports. */
export function persistLogs(storage: KeyValue) { reporter = createBufferedReporter(storage) }

export function reportLog(record: WebRecord) {
  const page = (globalThis as { location?: { pathname: string; search: string } }).location
  const ids = page ? pageIds(page) : {}
  reporter.report({ ...ids, ...record })
}

export const connectLogSink = (next: Sink) => reporter.connect(next)
export const logSource = () => reporter.source
/** Sends every kept record the device has not accepted yet. */
export const sendLogs = () => reporter.send()
/** Writes the kept records to storage now, for an app about to be suspended or closed. */
export const saveLogs = () => reporter.save()
/** Drops the kept records, as erasing the device does. */
export const clearLogs = () => reporter.clear()

export type LogFilter = {
  since_ms?: number
  until_ms?: number
  level?: LogLevel
  sources?: LogSource[]
  conversation_id?: string
  work_id?: string
  text?: string
  limit?: number
}

const levels: LogLevel[] = ["trace", "debug", "info", "warn", "error"]
const sources: LogSource[] = ["node", "web", "agent", "cli", "codex", "desktop", "mobile"]

export function parseLogs(value: unknown): LogRecord[] {
  const records = (value as { records?: unknown } | null)?.records
  if (!Array.isArray(records)) throw new Error("日志读取结果无效。")
  return records.flatMap((item): LogRecord[] => {
    const row = item as Partial<LogRecord> | null
    if (!row || typeof row.ts_ms !== "number" || typeof row.message !== "string" || typeof row.event !== "string"
      || !levels.includes(row.level as LogLevel) || !sources.includes(row.source as LogSource)) return []
    const id = (value: unknown) => typeof value === "string" ? value : undefined
    const fields = row.fields && typeof row.fields === "object" && !Array.isArray(row.fields) ? row.fields : undefined
    return [{ ts_ms: row.ts_ms, level: row.level as LogLevel, source: row.source as LogSource, event: row.event, message: row.message,
      conversation_id: id(row.conversation_id), work_id: id(row.work_id), thread_id: id(row.thread_id), fields }]
  })
}
