import { describe, expect, it } from 'vitest'
import { deepLinkFromArgv, parseDeepLink } from '../src/main/deeplink.js'

const TICKET = 'vibeface1qq2ww3ee4'

describe('parseDeepLink', () => {
  it('accepts a bare ticket and the join link', () => {
    expect(parseDeepLink(TICKET)).toEqual({ kind: 'join', ticket: TICKET })
    expect(parseDeepLink(` vibeface://join/${TICKET} `)).toEqual({ kind: 'join', ticket: TICKET })
  })

  it('refuses anything that is not an invite', () => {
    for (const hostile of [
      'file:///etc/passwd',
      'vibeface://run?cmd=/bin/sh',
      `vibeface://join?link=${TICKET}`,
      `vibeface://join/${TICKET}?data-dir=/tmp/other`,
      `vibeface://join/${TICKET}/../../etc/passwd`,
      `https://vb.example.com/join#${TICKET}`,
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
    const found = deepLinkFromArgv(['--profile-dir', '/tmp/profile', `vibeface://join/${TICKET}`])
    expect(found).toEqual({ kind: 'join', ticket: TICKET })
  })

  it('never reads a path or a flag value as a link', () => {
    expect(deepLinkFromArgv(['--profile-dir', '/tmp/profile', '--attach']).kind).toBe('ignored')
    expect(deepLinkFromArgv(['--runtime=/tmp/evil/vibeface-core']).kind).toBe('ignored')
  })
})
