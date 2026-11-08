import assert from 'node:assert/strict'
import test from 'node:test'
import { joinDestination, linkTarget, openedJoinLink } from './switch-network.ts'

const mine = 'a'.repeat(64), theirs = 'b'.repeat(64)
const tickets: Record<string, string> = { vibeface1mine: JSON.stringify({ entry_id: mine }), vibeface1theirs: JSON.stringify({ entry_id: theirs }) }
const decode = (ticket: string) => { const value = tickets[ticket]; if (!value) throw new Error('bad ticket'); return value }

test('a link from a device of this network stays in it', () => {
  assert.equal(linkTarget('https://entry.example/join#vibeface1mine', decode, [theirs, mine]), 'current')
  assert.equal(linkTarget(' vibeface1mine ', decode, [mine]), 'current')
})

test('an unknown entry stays distinct without proving which network it belongs to', () => {
  assert.equal(linkTarget('https://entry.example/join#vibeface1theirs', decode, [mine]), 'other')
  assert.equal(linkTarget('vibeface1theirs', decode, []), 'other')
})

test('only an unambiguous join shell carries a valid inner invitation', () => {
  const link = 'https://entry.example/join#vibeface1theirs'
  const shell = `vibeface://join?link=${encodeURIComponent(link)}`
  assert.equal(openedJoinLink(shell), link)
  assert.deepEqual(joinDestination(link, decode), { entryId: theirs, address: 'https://entry.example' })
  assert.deepEqual(joinDestination(shell, decode), { entryId: theirs, address: 'https://entry.example' })
  assert.equal(linkTarget(`${shell}&recover=${mine}`, decode, []), null)
  for (const value of [null, '', `https://attacker.example/?link=${encodeURIComponent(link)}`, shell.replace('://join', '://other'), `${shell}&recover=${mine}`, `${shell}&link=vibeface1mine`, `${shell}#fragment`, shell.replace('://join?', '://join/path?'), 'vibeface://join?link=broken']) assert.equal(openedJoinLink(value), null, String(value))
  assert.equal(joinDestination('ftp://entry.example/join#vibeface1theirs', decode), null)
  assert.equal(joinDestination('vibeface1invalid', () => JSON.stringify({ entry_id: '../wrong' })), null)
})

test('text that is no join link leads nowhere', () => {
  for (const text of ['', 'https://entry.example/join', 'https://entry.example/other#vibeface1mine', 'vibeface1broken']) assert.equal(linkTarget(text, decode, [mine]), null, text)
})
