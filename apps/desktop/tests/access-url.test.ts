import { describe, expect, it } from 'vitest'
import { normalizeAccessUrl, parseLocalListen } from '../src/main/access-url.js'

describe('parseLocalListen', () => {
  it('parses ipv4', () => {
    expect(parseLocalListen('127.0.0.1:4791')).toEqual({ host: '127.0.0.1', port: 4791 })
  })
  it('parses bracket ipv6', () => {
    expect(parseLocalListen('[::1]:4791')).toEqual({ host: '::1', port: 4791 })
  })
})

describe('normalizeAccessUrl', () => {
  it('rewrites localhost access port to local_listen', () => {
    const out = normalizeAccessUrl('http://localhost:4790/join#x', { host: '127.0.0.1', port: 4791 })
    expect(out).toContain(':4791/')
  })
  it('preserves fragment when rewriting port', () => {
    const out = normalizeAccessUrl('http://localhost:4790/#local=tok&node=id1', { host: '127.0.0.1', port: 4791 })
    expect(out).toContain(':4791/')
    expect(out).toContain('local=tok')
  })
})
