import assert from 'node:assert/strict'
import test from 'node:test'
import type { Entrance } from '../lib/entrances.ts'
import type { ConnectionEntry } from '../lib/node.ts'
import { joinTicket } from '../lib/transport.ts'
import { recoveryEntry, recoveryLink, recoveryTarget } from './recovery-link.ts'

const mac = 'a'.repeat(64), pi = 'b'.repeat(64)
const entrance = (device_id: string, gateway_url: string | null, extra: Partial<Entrance> = {}): Entrance =>
  ({ device_id, seq: 1, gateway_url, public: false, direct: [], last_ok_ms: 0, ...extra })

test('a recovery link carries only the computer to reach, and the phone reads back the same entry', () => {
  const web: ConnectionEntry = { kind: 'gateway', address: 'https://home.example.com', device_id: mac }
  const link = recoveryLink(web)
  assert.equal(link, `vibeface://join?recover=${mac}&address=https%3A%2F%2Fhome.example.com`)
  assert.deepEqual(recoveryTarget(link), web)
  assert.deepEqual(recoveryTarget(`  ${link}\n`), web, 'a pasted link may come with spaces around it')
  const lan: ConnectionEntry = { kind: 'lan', addrs: ['192.168.1.4:7001', '10.0.0.2:7001'], device_id: pi }
  assert.deepEqual(recoveryTarget(recoveryLink(lan)), lan)
})

test('only a recovery link fills in the computer: join links, broken ids and other schemes do not', () => {
  assert.equal(recoveryTarget('vibeface://join?link=vibeface1test'), null)
  assert.equal(joinTicket(recoveryLink({ kind: 'gateway', address: 'https://home.example.com', device_id: mac })), null, 'and a join form does not take it for an invitation')
  assert.equal(recoveryTarget(`vibeface://join?recover=${'z'.repeat(64)}&address=https%3A%2F%2Fhome.example.com`), null)
  assert.equal(recoveryTarget(`https://home.example.com/join?recover=${mac}&address=https%3A%2F%2Fhome.example.com`), null)
  assert.equal(recoveryTarget(`vibeface://join?recover=${mac}&address=https%3A%2F%2Fhome.example.com%2Fpath`), null)
  assert.equal(recoveryTarget(`vibeface://join?recover=${mac}`), null, 'a computer with nowhere to reach it is no target')
  assert.equal(recoveryTarget('设备编号'), null)
})

test('the phone is sent to a public entrance first, never to this computer\'s own loopback', () => {
  const local: ConnectionEntry = { kind: 'local', address: 'http://127.0.0.1:4790', device_id: mac }
  const list = [entrance(mac, 'http://127.0.0.1:4790', { direct: ['192.168.1.4:7001'] }), entrance(pi, 'https://relay.example.com', { public: true })]
  assert.deepEqual(recoveryEntry(list, local), { kind: 'gateway', address: 'https://relay.example.com', device_id: pi })
  assert.deepEqual(recoveryEntry([entrance(mac, 'https://mac.example.com', { public: true }), ...list], local), { kind: 'gateway', address: 'https://mac.example.com', device_id: mac }, 'the connected computer wins among public ones')
})

test('with no public entrance a LAN web address, then where the computer listens, is what the phone gets', () => {
  const lanWeb: ConnectionEntry = { kind: 'gateway', address: 'http://192.168.1.4:4790', device_id: mac }
  assert.deepEqual(recoveryEntry([], lanWeb), lanWeb)
  const local: ConnectionEntry = { kind: 'local', address: 'http://localhost:4790', device_id: mac }
  assert.deepEqual(recoveryEntry([entrance(mac, 'http://localhost:4790', { direct: ['192.168.1.4:7001'] })], local), { kind: 'lan', addrs: ['192.168.1.4:7001'], device_id: mac })
  assert.equal(recoveryEntry([entrance(mac, 'http://localhost:4790')], local), null, 'nothing a phone can reach gives no QR code')
})
