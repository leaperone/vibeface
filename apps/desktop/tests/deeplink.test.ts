import { describe, expect, it } from 'vitest'
import { parseDeepLink } from '../src/main/deeplink.js'

describe('parseDeepLink', () => {
  it('accepts vibeface join', () => {
    const r = parseDeepLink('vibeface://join?link=http%3A%2F%2F127.0.0.1%3A4790%2Fjoin%23abc')
    expect(r.kind === 'join' && r.link.length > 0).toBe(true)
  })
  it('accepts http join with hash', () => {
    const r = parseDeepLink('http://127.0.0.1:4790/join#vibeface1test')
    expect(r.kind).toBe('join')
  })
  it('rejects file', () => {
    expect(parseDeepLink('file:///tmp/x').kind).toBe('invalid')
  })
})
