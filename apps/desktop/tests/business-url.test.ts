import { describe, expect, it } from 'vitest'
import { businessNavigation, businessUrl, parsePrivateOrigin } from '../src/main/business-url.js'
import type { Access, Inspect } from '../src/main/control-types.js'

const DEVICE = 'a'.repeat(64)
const TOKEN = 'b'.repeat(64)

const inspect = (overrides: Partial<Inspect> = {}): Inspect => ({
  status: 'running',
  deviceId: DEVICE,
  dataDir: '/tmp/node',
  localListen: '127.0.0.1:4791',
  listen: '127.0.0.1:4790',
  webEnabled: true,
  publicEntry: false,
  coreVersion: '0.0.1',
  error: null,
  ...overrides,
})

const access = (url = `http://localhost:4790/#local=${TOKEN}&node=${DEVICE}`, deviceId = DEVICE): Access => ({
  kind: 'local',
  url,
  deviceId,
})

describe('parsePrivateOrigin', () => {
  it('reads the loopback host and port Core reported', () => {
    expect(parsePrivateOrigin('127.0.0.1:4791')).toEqual({
      host: '127.0.0.1',
      port: 4791,
      origin: 'http://127.0.0.1:4791',
    })
  })

  it('refuses an address that is not loopback', () => {
    expect(() => parsePrivateOrigin('192.168.1.10:4791')).toThrow(/loopback/)
  })

  it('refuses an unusable port', () => {
    expect(() => parsePrivateOrigin('127.0.0.1:70000')).toThrow(/port/)
  })
})

describe('businessUrl', () => {
  it('moves the access URL onto the private local_listen origin', () => {
    expect(businessUrl(access(), inspect())).toBe(`http://127.0.0.1:4791/#local=${TOKEN}&node=${DEVICE}`)
  })

  it('never loads the public 4790 listener for a local session', () => {
    expect(businessUrl(access(), inspect())).not.toContain('4790')
  })

  it('keeps the path and query Core produced', () => {
    const url = businessUrl(
      access(`http://127.0.0.1:4790/join?from=cli#local=${TOKEN}&node=${DEVICE}`),
      inspect()
    )
    expect(url).toBe(`http://127.0.0.1:4791/join?from=cli#local=${TOKEN}&node=${DEVICE}`)
  })

  it('follows a non-default local_listen port', () => {
    expect(businessUrl(access(), inspect({ localListen: '127.0.0.1:5801' }))).toContain(':5801/')
  })

  it('keeps a bootstrap fragment untouched', () => {
    const url = businessUrl(access('http://localhost:4790/#bootstrap=eyJhIjoxfQ'), inspect())
    expect(url).toBe('http://127.0.0.1:4791/#bootstrap=eyJhIjoxfQ')
  })

  it('rejects a local entry minted for another node', () => {
    const other = 'c'.repeat(64)
    expect(() => businessUrl(access(`http://localhost:4790/#local=${TOKEN}&node=${other}`), inspect())).toThrow(
      /another node/
    )
  })

  it('rejects a token that is not a 64 character hex string', () => {
    expect(() => businessUrl(access(`http://localhost:4790/#local=short&node=${DEVICE}`), inspect())).toThrow(
      /local entry/
    )
  })

  it('rejects an access reply from a different node than the inspected one', () => {
    expect(() => businessUrl(access(undefined, 'd'.repeat(64)), inspect())).toThrow(/different nodes/)
  })

  it('rejects an access URL that is not loopback', () => {
    expect(() => businessUrl(access('http://198.51.100.7:4790/'), inspect())).toThrow(/loopback/)
  })

  it('passes a public entry through without rewriting it', () => {
    const url = businessUrl(
      { kind: 'public_entry', url: 'https://vb.example.com/', deviceId: DEVICE },
      inspect({ publicEntry: true })
    )
    expect(url).toBe('https://vb.example.com/')
  })

  it('refuses a public entry published over plain http', () => {
    expect(() =>
      businessUrl({ kind: 'public_entry', url: 'http://vb.example.com/', deviceId: DEVICE }, inspect({ publicEntry: true }))
    ).toThrow(/HTTPS/)
  })
})

describe('businessNavigation', () => {
  const invite = { ticket: 'vibeface1qq2ww3ee4' }

  it('opens an invite at the selected Node\'s join form', () => {
    expect(businessNavigation(access(), inspect(), invite)).toEqual({
      kind: 'business',
      url: 'http://127.0.0.1:4791/join#vibeface1qq2ww3ee4',
    })
    const publicAccess: Access = { kind: 'public_entry', url: 'https://vb.example.com/', deviceId: DEVICE }
    expect(businessNavigation(publicAccess, inspect({ publicEntry: true }), invite)).toEqual({
      kind: 'business',
      url: 'https://vb.example.com/join#vibeface1qq2ww3ee4',
    })
  })
})
