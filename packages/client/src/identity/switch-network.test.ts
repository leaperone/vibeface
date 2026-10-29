import assert from 'node:assert/strict'
import test from 'node:test'
import { linkTarget } from './switch-network.ts'

const mine = 'a'.repeat(64), theirs = 'b'.repeat(64)
const tickets: Record<string, string> = { vibeface1mine: JSON.stringify({ entry_id: mine }), vibeface1theirs: JSON.stringify({ entry_id: theirs }) }
const decode = (ticket: string) => { const value = tickets[ticket]; if (!value) throw new Error('bad ticket'); return value }

test('a link from a device of this network stays in it', () => {
  assert.equal(linkTarget('https://entry.example/join#vibeface1mine', decode, [theirs, mine]), 'current')
  assert.equal(linkTarget(' vibeface1mine ', decode, [mine]), 'current')
})

test('a link from any other device leads to another network, also when this one is unreachable', () => {
  assert.equal(linkTarget('https://entry.example/join#vibeface1theirs', decode, [mine]), 'other')
  assert.equal(linkTarget('vibeface1theirs', decode, []), 'other')
})

test('text that is no join link leads nowhere', () => {
  for (const text of ['', 'https://entry.example/join', 'https://entry.example/other#vibeface1mine', 'vibeface1broken']) assert.equal(linkTarget(text, decode, [mine]), null, text)
})
