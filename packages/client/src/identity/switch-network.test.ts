import assert from 'node:assert/strict'
import test from 'node:test'
import { joinDestination, linkTarget } from './switch-network.ts'

const mine = 'a'.repeat(64), theirs = 'b'.repeat(64)
const tickets: Record<string, string> = { vibeface1mine: JSON.stringify({ entry_id: mine }), vibeface1theirs: JSON.stringify({ entry_id: theirs }) }
const decode = (ticket: string) => { const value = tickets[ticket]; if (!value) throw new Error('bad ticket'); return value }

test('a link from a device of this network stays in it', () => {
  assert.equal(linkTarget('vibeface://join/vibeface1mine', decode, [theirs, mine]), 'current')
  assert.equal(linkTarget(' vibeface1mine ', decode, [mine]), 'current')
})

test('a link from any other device leads to another network, also when this one is unreachable', () => {
  assert.equal(linkTarget('vibeface://join/vibeface1theirs', decode, [mine]), 'other')
  assert.equal(linkTarget('vibeface1theirs', decode, []), 'other')
})

test('the destination is the entry device the ticket names, and only a valid one', () => {
  assert.equal(joinDestination('vibeface://join/vibeface1theirs', decode), theirs)
  assert.equal(joinDestination(' vibeface1theirs ', decode), theirs)
  assert.equal(joinDestination('vibeface1invalid', () => JSON.stringify({ entry_id: '../wrong' })), null)
  assert.equal(joinDestination('vibeface1invalid', () => JSON.stringify({ entry_id: 7 })), null)
  assert.equal(joinDestination('vibeface1invalid', () => JSON.stringify(theirs)), null)
  assert.equal(joinDestination('vibeface1invalid', () => 'not json'), null)
  assert.equal(linkTarget('vibeface1invalid', () => JSON.stringify({ entry_id: '../wrong' }), []), null)
})

test('text that is no join link leads nowhere', () => {
  const shell = `vibeface://join?link=${encodeURIComponent('vibeface1theirs')}`
  for (const text of ['', 'vibeface://join/', 'https://entry.example/join#vibeface1mine', 'vibeface1broken', shell, `vibeface://join/vibeface1theirs?recover=${mine}`, 'vibeface://join/vibeface1theirs#fragment']) {
    assert.equal(linkTarget(text, decode, [mine]), null, text)
    assert.equal(joinDestination(text, decode), null, text)
  }
})
