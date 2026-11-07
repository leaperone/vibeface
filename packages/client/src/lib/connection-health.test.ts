import test from 'node:test'
import assert from 'node:assert/strict'
import { connectionHealth } from './connection-health.ts'

test('connection status stays quiet for one blip and short reads', () => {
  assert.equal(connectionHealth({ disconnects: 1, readStartedAt: 100, now: 500, timedOut: false }), 'quiet')
})

test('repeated disconnects, timeout, and long reads surface recovery state', () => {
  assert.equal(connectionHealth({ disconnects: 2, readStartedAt: null, now: 0, timedOut: false }), 'recovering')
  assert.equal(connectionHealth({ disconnects: 0, readStartedAt: null, now: 0, timedOut: true }), 'recovering')
  assert.equal(connectionHealth({ disconnects: 0, readStartedAt: 1, now: 8001, timedOut: false }), 'recovering')
})
