import { describe, expect, it } from 'vitest'
import { parseArgs } from '../src/main/cli.js'

describe('parseArgs', () => {
  it('defaults to the real profile and a full session', () => {
    expect(parseArgs([])).toEqual({ profileDir: null, attachOnly: false })
  })

  it('reads an isolated profile directory in both spellings', () => {
    expect(parseArgs(['--profile-dir', '/tmp/run-1']).profileDir).toBe('/tmp/run-1')
    expect(parseArgs(['--profile-dir=/tmp/run-2']).profileDir).toBe('/tmp/run-2')
  })

  it('refuses a relative profile directory rather than writing beside the app', () => {
    expect(() => parseArgs(['--profile-dir', 'run-3'])).toThrow(/absolute/)
    expect(() => parseArgs(['--profile-dir'])).toThrow(/absolute/)
  })

  it('reads connection-only mode', () => {
    expect(parseArgs(['--attach']).attachOnly).toBe(true)
  })

  it('ignores Chromium and packaging flags it does not own', () => {
    const parsed = parseArgs(['--remote-debugging-port=9222', '--enable-logging', '--attach'])
    expect(parsed).toEqual({ profileDir: null, attachOnly: true })
  })
})
