import { describe, expect, it } from 'vitest'
import { CLIENT_ORIGIN } from '../src/shared/client-contract.js'
import {
  isTrustedClientCall,
  readBoolean,
  readExternalUrl,
  readNoArguments,
  readString,
} from '../src/main/ipc-guard.js'

const contents = { id: 'client-contents' }
const mainFrame = { id: 'client-main-frame' }
const client = { contents, mainFrame }

const call = (overrides: Partial<Parameters<typeof isTrustedClientCall>[0]> = {}) =>
  isTrustedClientCall(
    { sender: contents, senderFrame: mainFrame, frameUrl: `${CLIENT_ORIGIN}/settings`, frameParent: null, ...overrides },
    client
  )

describe('isTrustedClientCall', () => {
  it('accepts the client main frame at the fixed client origin', () => {
    expect(call()).toBe(true)
  })

  it('rejects another window’s WebContents', () => {
    expect(call({ sender: { id: 'other' } })).toBe(false)
  })

  it('rejects a subframe of the client', () => {
    expect(call({ senderFrame: { id: 'iframe' } })).toBe(false)
  })

  it('rejects the main frame once it has a parent', () => {
    expect(call({ frameParent: { id: 'parent' } })).toBe(false)
  })

  it('rejects a frame that navigated away from the client origin', () => {
    expect(call({ frameUrl: 'http://127.0.0.1:4791/' })).toBe(false)
  })

  it('rejects a frame whose URL could not be read', () => {
    expect(call({ frameUrl: null })).toBe(false)
  })

  it('rejects everything while no client window exists', () => {
    expect(
      isTrustedClientCall(
        { sender: contents, senderFrame: mainFrame, frameUrl: `${CLIENT_ORIGIN}/`, frameParent: null },
        { contents: null, mainFrame: null }
      )
    ).toBe(false)
  })
})

describe('payload readers', () => {
  it('rejects a non-string where a path is expected', () => {
    expect(() => readString(42, '数据目录', 1024)).toThrow(/text/)
    expect(() => readString({ toString: () => '/tmp' }, '数据目录', 1024)).toThrow(/text/)
  })

  it('rejects an empty or oversized string', () => {
    expect(() => readString('   ', '数据目录', 1024)).toThrow(/length/)
    expect(() => readString('x'.repeat(1025), '数据目录', 1024)).toThrow(/length/)
  })

  it('rejects a truthy non-boolean switch value', () => {
    expect(() => readBoolean('true', '设置值')).toThrow(/true or false/)
  })

  it('rejects extra arguments on a no-argument channel', () => {
    expect(() => readNoArguments(['/etc/passwd'], 'desktop:get-status')).toThrow(/no arguments/)
  })

  it('opens only http and https links', () => {
    expect(readExternalUrl('https://example.com/a')).toBe('https://example.com/a')
    for (const blocked of [
      'file:///etc/passwd',
      'vibeface://join/x',
      'javascript:alert(1)',
      'ms-msdt:/id',
      'smb://host/share',
    ]) {
      expect(() => readExternalUrl(blocked)).toThrow()
    }
  })
})
