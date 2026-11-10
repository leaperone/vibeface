import type { Readable } from 'node:stream'

/**
 * Read a stream until it ends. A chunk that would pass `limit` fails closed
 * with the caller's message and does not return a partial result.
 * Callers keep their own messages: control replies must stay distinct from
 * managed-node results, and this helper never inspects stderr.
 */
export async function readBounded(stream: Readable, limit: number, overflow: string): Promise<string> {
  const chunks: Buffer[] = []
  let size = 0
  for await (const chunk of stream) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), 'utf8')
    size += bytes.length
    if (size > limit) throw new Error(overflow)
    chunks.push(bytes)
  }
  return Buffer.concat(chunks, size).toString('utf8')
}
