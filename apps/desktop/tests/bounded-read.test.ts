import { Readable } from 'node:stream'
import { describe, expect, it } from 'vitest'
import { readBounded } from '../src/main/bounded-read.js'

describe('readBounded', () => {
  it('joins buffer and string chunks that stay within the limit', async () => {
    const stream = Readable.from([Buffer.from('hello'), ' ', Buffer.from('world')])
    await expect(readBounded(stream, 11, 'overflow')).resolves.toBe('hello world')
  })

  it('accepts a result that lands exactly on the limit', async () => {
    const stream = Readable.from([Buffer.from('hello')])
    await expect(readBounded(stream, 5, 'overflow')).resolves.toBe('hello')
  })

  it('fails closed with the caller message and does not return a partial result', async () => {
    const stream = Readable.from([Buffer.from('hello'), Buffer.from('!')])
    await expect(readBounded(stream, 5, 'core control response exceeds its size limit')).rejects.toThrow(
      'core control response exceeds its size limit',
    )
  })

  it('uses an empty string for a stream that ends without bytes', async () => {
    await expect(readBounded(Readable.from([]), 8, 'overflow')).resolves.toBe('')
  })
})
