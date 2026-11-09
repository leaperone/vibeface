import { describe, expect, it } from 'vitest'
import { ActivationQueue } from '../src/main/activation-queue.js'

const waitFor = async (check: () => boolean, timeoutMs = 1000): Promise<void> => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (check()) return
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
  throw new Error('activation queue never reached the expected state')
}

describe('ActivationQueue', () => {
  it('queues cold-start invites, deduplicates duplicates, and keeps deferred invites', async () => {
    const ticket = 'vibeface1coldstart'
    let readyForLocalJoin = false
    const handled: string[] = []
    const queue = new ActivationQueue(async (link) => {
      handled.push(link.ticket)
      return readyForLocalJoin ? 'handled' : 'defer'
    })

    expect(queue.enqueue({ kind: 'join', ticket })).toBe(true)
    expect(queue.enqueue({ kind: 'join', ticket })).toBe(true)
    expect(queue.enqueue({ kind: 'ignored' })).toBe(false)
    expect(queue.pending()).toBe(1)

    await queue.flush()
    expect(handled).toEqual([])

    queue.setReady()
    await queue.flush()
    expect(handled).toEqual([ticket])
    expect(queue.pending()).toBe(1)

    readyForLocalJoin = true
    await queue.flush()
    expect(handled).toEqual([ticket, ticket])
    expect(queue.pending()).toBe(0)
  })

  it('processes ready-time activations immediately', async () => {
    const ticket = 'vibeface1readyevent'
    const handled: string[] = []
    const queue = new ActivationQueue(async (link) => {
      handled.push(link.ticket)
      return 'handled'
    })
    queue.setReady()

    expect(queue.enqueue({ kind: 'join', ticket })).toBe(true)
    await waitFor(() => handled.length === 1)
    expect(handled).toEqual([ticket])
    expect(queue.pending()).toBe(0)
  })

  it('bounds in-memory pending invites', () => {
    const queue = new ActivationQueue(async () => 'defer', 2)
    expect(queue.enqueue({ kind: 'join', ticket: 'vibeface1a' })).toBe(true)
    expect(queue.enqueue({ kind: 'join', ticket: 'vibeface1b' })).toBe(true)
    expect(queue.enqueue({ kind: 'join', ticket: 'vibeface1c' })).toBe(false)
    expect(queue.pending()).toBe(2)
  })
})
