import { parseMarkdownResponse, type MarkdownRequest } from "./markdown-protocol.ts"

export type MarkdownResult = { kind: "rendered"; text: string; html: string } | { kind: "unavailable" } | { kind: "busy" }
export type MarkdownWorker = {
  postMessage: (request: MarkdownRequest) => void
  terminate: () => void
  onmessage: ((event: { data: unknown }) => void) | null
  onerror: (() => void) | null
  onmessageerror: (() => void) | null
}

type Owner = { active: boolean; version: number }
type Job = { owner: Owner; id: number; text: string; due: number; streaming: boolean; receive: (result: MarkdownResult) => void }
const STREAM_INTERVAL_MS = 60
export const MAX_PENDING_MARKDOWN = 32
const defaultClock = {
  now: () => performance.now(),
  setTimeout: (run: () => void, delay: number) => setTimeout(run, delay),
  clearTimeout: (timer: ReturnType<typeof setTimeout>) => clearTimeout(timer),
}

/** One in-flight parse and at most one queued snapshot per message. No HTML/history cache. */
export class MarkdownScheduler {
  private worker: MarkdownWorker | undefined
  private unavailable = false
  private pending = new Map<Owner, Job>()
  private running: Job | undefined
  private timer: ReturnType<typeof setTimeout> | undefined
  private sequence = 0
  private subscribers = 0
  private readonly createWorker: () => MarkdownWorker | undefined
  private readonly clock: typeof defaultClock

  constructor(createWorker: () => MarkdownWorker | undefined, clock = defaultClock) {
    this.createWorker = createWorker
    this.clock = clock
  }

  connect(receive: (result: MarkdownResult) => void) {
    const owner: Owner = { active: true, version: 0 }
    this.subscribers++
    const cancel = () => {
      owner.version = ++this.sequence
      this.pending.delete(owner)
    }
    return {
      update: (text: string, streaming: boolean) => {
        if (!owner.active) return
        const previous = this.pending.get(owner)
        owner.version = ++this.sequence
        if (this.unavailable) {
          receive({ kind: "unavailable" })
          return
        }
        if (!previous && this.pending.size >= MAX_PENDING_MARKDOWN) {
          receive({ kind: "busy" })
          return
        }
        this.pending.set(owner, {
          owner, id: owner.version, text, receive, streaming,
          due: streaming ? previous?.due ?? this.clock.now() + STREAM_INTERVAL_MS : this.clock.now(),
        })
        this.pump()
      },
      cancel,
      dispose: () => {
        if (!owner.active) return
        cancel()
        owner.active = false
        if (--this.subscribers === 0) {
          this.stopWorker()
          this.running = undefined
          this.clearTimer()
        }
      },
    }
  }

  private deliver(job: Job, result: MarkdownResult) {
    if (job.owner.active && job.owner.version === job.id) job.receive(result)
  }

  private clearTimer() {
    if (this.timer !== undefined) this.clock.clearTimeout(this.timer)
    this.timer = undefined
  }

  private fail() {
    this.unavailable = true
    this.stopWorker()
    this.clearTimer()
    if (this.running) this.deliver(this.running, { kind: "unavailable" })
    this.running = undefined
    const jobs = [...this.pending.values()]
    this.pending.clear()
    for (const job of jobs) this.deliver(job, { kind: "unavailable" })
  }

  private stopWorker() {
    const worker = this.worker
    this.worker = undefined
    if (!worker) return
    worker.onmessage = null
    worker.onerror = null
    worker.onmessageerror = null
    worker.terminate()
  }

  private pump() {
    if (this.running) return
    this.clearTimer()
    let next: Job | undefined
    for (const job of this.pending.values()) {
      if (!next || (next.streaming && !job.streaming) || (next.streaming === job.streaming && job.due < next.due)) next = job
    }
    if (!next) return
    const delay = next.due - this.clock.now()
    if (delay > 0) {
      this.timer = this.clock.setTimeout(() => this.pump(), delay)
      return
    }
    try {
      if (!this.worker) {
        this.worker = this.createWorker()
        if (!this.worker) return this.fail()
        const worker = this.worker
        worker.onmessage = ({ data }) => {
          if (this.worker !== worker) return
          const response = parseMarkdownResponse(data)
          const job = this.running
          if (!response || !job || response.id !== job.id) return this.fail()
          this.running = undefined
          this.deliver(job, response.kind === "rendered" ? { kind: "rendered", text: job.text, html: response.html } : { kind: "unavailable" })
          this.pump()
        }
        worker.onerror = () => { if (this.worker === worker) this.fail() }
        worker.onmessageerror = () => { if (this.worker === worker) this.fail() }
      }
      this.pending.delete(next.owner)
      this.running = next
      this.worker.postMessage({ id: next.id, text: next.text })
    } catch {
      this.fail()
    }
  }
}

export const markdownScheduler = new MarkdownScheduler(() => {
  if (typeof window === "undefined" || typeof Worker === "undefined") return
  const worker = new Worker(new URL("./markdown-worker.ts", import.meta.url), { type: "module" })
  const transport: MarkdownWorker = {
    postMessage: (request) => worker.postMessage(request), terminate: () => worker.terminate(),
    onmessage: null, onerror: null, onmessageerror: null,
  }
  worker.onmessage = (event: MessageEvent<unknown>) => transport.onmessage?.({ data: event.data })
  worker.onerror = () => transport.onerror?.()
  worker.onmessageerror = () => transport.onmessageerror?.()
  return transport
})
