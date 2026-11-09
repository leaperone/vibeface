import assert from 'node:assert/strict'
import test from 'node:test'
import { isDesktopHost } from './desktop-node-bridge.ts'

test('desktop node settings are only exposed inside the desktop host', () => {
  assert.equal(isDesktopHost(), false)
})
