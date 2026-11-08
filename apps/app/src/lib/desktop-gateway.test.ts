import assert from 'node:assert/strict'
import test from 'node:test'
import { desktopGatewayCarrier } from './desktop-local-page.ts'
import type { LanStream } from '@vibeface/client/lib/transport.ts'

const entry = { kind: 'gateway' as const, address: 'http://127.0.0.1:4790', device_id: 'a'.repeat(64) }
const channel: LanStream = { send() {}, async recv() { return null }, close() {} }

test('public invitation origins reach the native broker before browser fallback', async () => {
  const calls: unknown[][] = []
  const gateway = desktopGatewayCarrier({ async openGateway(...args) { calls.push(args); return 'native-stream' } }, () => channel)
  assert.equal(await gateway(entry, entry.device_id, 1000), channel)
  assert.deepEqual(calls, [[entry.device_id, 'http://127.0.0.1:4790', 1000]])
})

test('a non-local Gateway declined by the broker uses normal browser transport', async () => {
  let converted = false
  const gateway = desktopGatewayCarrier({ async openGateway() { return null } }, () => { converted = true; return channel })
  assert.equal(await gateway({ ...entry, address: 'https://gateway.example' }, entry.device_id, 1000), null)
  assert.equal(converted, false)
})

test('a native transport failure propagates instead of silently retrying another carrier', async () => {
  const gateway = desktopGatewayCarrier({ async openGateway() { throw new Error('local connection failed') } }, () => channel)
  await assert.rejects(gateway(entry, entry.device_id, 1000), /local connection failed/)
})
