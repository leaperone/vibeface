import { describe, expect, it } from 'vitest'
import { deepLinkFromArgv, parseDeepLink } from '../src/main/deeplink.js'

const TICKET = 'vibeface1qq2ww3ee4'

describe('parseDeepLink', () => {
  it('accepts a bare ticket', () => {
    expect(parseDeepLink(TICKET)).toEqual({ kind: 'join', ticket: TICKET, origin: null })
  })

  it('accepts a published join URL and keeps its entry origin', () => {
    expect(parseDeepLink(`https://vb.example.com/join#${TICKET}`)).toEqual({
      kind: 'join',
      ticket: TICKET,
      origin: 'https://vb.example.com',
    })
  })

  it('accepts the app wrapper around either form', () => {
    expect(parseDeepLink(`vibeface://join?link=${encodeURIComponent(`http://127.0.0.1:4790/join#${TICKET}`)}`)).toEqual({
      kind: 'join',
      ticket: TICKET,
      origin: 'http://127.0.0.1:4790',
    })
    expect(parseDeepLink(`vibeface://join?link=${TICKET}`)).toEqual({ kind: 'join', ticket: TICKET, origin: null })
  })

  it('refuses a wrapper nested inside a wrapper', () => {
    const inner = encodeURIComponent(`vibeface://join?link=${TICKET}`)
    expect(parseDeepLink(`vibeface://join?link=${inner}`).kind).toBe('ignored')
  })

  it('refuses anything that is not an invite', () => {
    for (const hostile of [
      'file:///etc/passwd',
      'vibeface://run?cmd=/bin/sh',
      'vibeface://join?link=file:///etc/passwd',
      `vibeface://join?data-dir=/tmp/other#${TICKET}`,
      'https://vb.example.com/admin#vibeface1abc',
      `https://vb.example.com/join#${TICKET.toUpperCase()}`,
      'https://vb.example.com/join#../../etc/passwd',
      'javascript:alert(1)',
      '',
      `https://vb.example.com/join#${'a'.repeat(5000)}`,
    ]) {
      expect(parseDeepLink(hostile).kind, hostile).toBe('ignored')
    }
  })
})

describe('deepLinkFromArgv', () => {
  it('finds the invite among normal arguments', () => {
    const found = deepLinkFromArgv(['--profile-dir', '/tmp/profile', `vibeface://join?link=${TICKET}`])
    expect(found).toEqual({ kind: 'join', ticket: TICKET, origin: null })
  })

  it('never reads a path or a flag value as a link', () => {
    expect(deepLinkFromArgv(['--profile-dir', '/tmp/profile', '--attach']).kind).toBe('ignored')
    expect(deepLinkFromArgv(['--runtime=/tmp/evil/vibeface-core']).kind).toBe('ignored')
  })
})
