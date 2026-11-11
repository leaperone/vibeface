import type { DeepLink } from './deeplink.js'

type JoinActivation = Extract<DeepLink, { kind: 'join' }>
type ActivationResult = 'handled' | 'defer'

const MAX_PENDING = 32

function activationKey(link: JoinActivation): string {
  return link.ticket
}

/**
 * Queues join activations until the main process is ready, deduplicates duplicates from
 * argv/open-url/second-instance, and retries deferred invites later without writing to disk.
 */
export class ActivationQueue {
  private readonly queue: JoinActivation[] = []
  private readonly seen = new Set<string>()
  private ready = false
  private flushing = false

  constructor(
    private readonly consume: (link: JoinActivation) => Promise<ActivationResult>,
    private readonly maxPending = MAX_PENDING
  ) {}

  isReady(): boolean {
    return this.ready
  }

  pending(): number {
    return this.queue.length
  }

  enqueue(link: DeepLink): boolean {
    if (link.kind !== 'join') return false
    const key = activationKey(link)
    if (this.seen.has(key)) return true
    if (this.queue.length >= this.maxPending) return false
    this.queue.push(link)
    this.seen.add(key)
    if (this.ready) void this.flush()
    return true
  }

  setReady(): void {
    if (this.ready) return
    this.ready = true
    void this.flush()
  }

  async flush(): Promise<void> {
    if (!this.ready || this.flushing) return
    this.flushing = true
    try {
      while (this.queue.length > 0) {
        const next = this.queue[0]
        if (!next) return
        const outcome = await this.consume(next)
        if (outcome === 'defer') return
        this.queue.shift()
        this.seen.delete(activationKey(next))
      }
    } finally {
      this.flushing = false
    }
  }
}
