import { describe, expect, it } from 'vitest'
import { LAUNCHER_URL } from '../src/shared/contract.js'
import {
  isTrustedLauncherCall,
  readBoolean,
  readExternalUrl,
  readNoArguments,
  readString,
} from '../src/main/ipc-guard.js'

const contents = { id: 'launcher-contents' }
const mainFrame = { id: 'launcher-main-frame' }
const launcher = { contents, mainFrame }

const call = (overrides: Partial<Parameters<typeof isTrustedLauncherCall>[0]> = {}) =>
  isTrustedLauncherCall(
    { sender: contents, senderFrame: mainFrame, frameUrl: LAUNCHER_URL, frameParent: null, ...overrides },
    launcher
  )

describe('isTrustedLauncherCall', () => {
  it('accepts the launcher main frame at its own URL', () => {
    expect(call()).toBe(true)
  })

  it('rejects another window’s WebContents', () => {
    expect(call({ sender: { id: 'business' } })).toBe(false)
  })

  it('rejects a subframe of the launcher', () => {
    expect(call({ senderFrame: { id: 'iframe' } })).toBe(false)
  })

  it('rejects the main frame once it has a parent', () => {
    expect(call({ frameParent: { id: 'parent' } })).toBe(false)
  })

  it('rejects a frame that navigated away from the launcher URL', () => {
    expect(call({ frameUrl: 'http://127.0.0.1:4791/' })).toBe(false)
  })

  it('rejects a frame whose URL could not be read', () => {
    expect(call({ frameUrl: null })).toBe(false)
  })

  it('rejects everything while no launcher window exists', () => {
    expect(
      isTrustedLauncherCall(
        { sender: contents, senderFrame: mainFrame, frameUrl: LAUNCHER_URL, frameParent: null },
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
    expect(() => readNoArguments(['/etc/passwd'], 'launcher:get-status')).toThrow(/no arguments/)
  })

  it('opens only http and https links', () => {
    expect(readExternalUrl('https://example.com/a')).toBe('https://example.com/a')
    for (const blocked of [
      'file:///etc/passwd',
      'vibeface://join?link=x',
      'javascript:alert(1)',
      'ms-msdt:/id',
      'smb://host/share',
    ]) {
      expect(() => readExternalUrl(blocked)).toThrow()
    }
  })
})
