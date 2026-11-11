import assert from 'node:assert/strict'
import test from 'node:test'
import { isDesktopHost } from './desktop-node-bridge.ts'

test('desktop node settings are only exposed inside the desktop host', () => {
  const host = globalThis as { window?: unknown }
  const before = Object.getOwnPropertyDescriptor(host, 'window')
  try {
    assert.equal(isDesktopHost(), false)
    host.window = { vibefaceClient: {} }
    assert.equal(isDesktopHost(), false)
    host.window = { vibefaceClient: { desktop: {} } }
    assert.equal(isDesktopHost(), true)
  } finally {
    if (before) Object.defineProperty(host, 'window', before)
    else delete host.window
  }
})
