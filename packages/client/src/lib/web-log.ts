/** Browser records for the connected device's log: errors, failed requests and user actions. */
export type LogLevel = "trace" | "debug" | "info" | "warn" | "error"
export type LogSource = "node" | "web" | "agent" | "cli" | "codex" | "cursor"

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
type Sink = (records: WebRecord[]) => Promise<unknown>

/** The device rejects a record over 8 KiB of JSON and a batch over 20. */
export const MAX_RECORD_BYTES = 7000
export const MAX_BATCH = 20
const MAX_QUEUED = 100

const bytes = (value: unknown) => new TextEncoder().encode(JSON.stringify(value)).length

/** Shrinks a record until the device accepts it: fields first, then the message. */
export function fit(record: WebRecord): WebRecord {
  if (bytes(record) <= MAX_RECORD_BYTES) return record
  let fitted: WebRecord = { ...record, fields: record.fields && { truncated: true } }
  let message = record.message
  while (bytes(fitted) > MAX_RECORD_BYTES) {
    message = message.slice(0, Math.floor(message.length * 0.8))
    fitted = { ...fitted, message: `${message}…` }
  }
  return fitted
}

/** The conversation and task the page shows, from `?conversation=` and `/work/<id>`. */
export function pageIds(location: { pathname: string; search: string }): Pick<WebRecord, "conversation_id" | "work_id"> {
  const conversation = new URLSearchParams(location.search).get("conversation")
  const work = /\/work\/([^/]+)/.exec(location.pathname)?.[1]
  const valid = (id: string | null | undefined) => id && /^[A-Za-z0-9_.:-]{1,128}$/.test(id) ? id : undefined
  return { conversation_id: valid(conversation), work_id: valid(work && decodeURIComponent(work)) }
}

/** Sends as soon as a device is connected; records that arrive during a send go in the next batch. */
export function createReporter() {
  const queue: WebRecord[] = []
  let sink: Sink | null = null
  let sending = false
  const schedule = () => { if (sink && queue.length && !sending) void Promise.resolve().then(flush) }
  async function flush() {
    if (!sink || sending || !queue.length) return
    sending = true
    const batch = queue.splice(0, MAX_BATCH)
    // A record that cannot be delivered is dropped; reporting its failure would loop.
    try { await sink(batch) } catch { /* dropped */ } finally { sending = false }
    schedule()
  }
  return {
    report(record: WebRecord) {
      queue.push(fit(record))
      if (queue.length > MAX_QUEUED) queue.splice(0, queue.length - MAX_QUEUED)
      schedule()
    },
    /** Records wait in memory until a device connects; returns the detach function. */
    connect(next: Sink) {
      sink = next
      schedule()
      return () => { if (sink === next) sink = null }
    },
    flush,
    pending: () => queue.length,
  }
}

const reporter = createReporter()

export function reportLog(record: WebRecord) {
  const page = (globalThis as { location?: { pathname: string; search: string } }).location
  const ids = page ? pageIds(page) : {}
  reporter.report({ ...ids, ...record })
}

export const connectLogSink = reporter.connect

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
const sources: LogSource[] = ["node", "web", "agent", "cli", "codex", "cursor"]

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
