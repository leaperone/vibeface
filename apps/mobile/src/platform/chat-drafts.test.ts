import assert from 'node:assert/strict'
import test from 'node:test'
import { createChatDrafts, draftScopeKey, type DraftScope } from './chat-drafts.ts'
import type { Attachment } from '../data/types.ts'

const scope: DraftScope = { networkId: 'network', deviceId: 'device', conversationId: 'conversation', inputTarget: 'main' }
const selected: Attachment = { uri: 'picker:report', name: 'report.txt', size: 3, mimeType: 'text/plain' }
function fixture() {
  const storage = new Map<string, string>(), copies = new Map<string, Attachment>(), released: string[] = []
  const source = new Set([selected.uri])
  let count = 0, sequence = 0, fail = false
  let gate: Promise<void> | undefined
  const options = {
    storage: {
      get: (key: string) => storage.get(key),
      set(key: string, value: string) { if (fail) throw new Error('disk full'); storage.set(key, value) },
      remove(key: string) { storage.delete(key) },
    },
    revision: () => `revision-${++sequence}`,
    files: {
      async retain(file: Attachment) { await gate; const copy = { ...file, uri: `private:${++count}` }; copies.set(copy.uri, copy); return copy },
      exists: (uri: string) => source.has(uri) || copies.has(uri),
      owns: (uri: string) => uri.startsWith('private:'),
      release(uri: string) { released.push(uri); copies.delete(uri) },
    },
  }
  return { repository: createChatDrafts(options), storage, copies, released, count: () => count,
    restart: () => createChatDrafts(options), fail(value: boolean) { fail = value }, delay(promise: Promise<void> | undefined) { gate = promise } }
}

test('full drafts restore after restart and isolate every identity and input target', async () => {
  const f = fixture()
  const saved = await f.repository.write(scope, { text: 'keep me', files: [selected] })
  assert.notEqual(saved.files[0].uri, selected.uri)
  assert.deepEqual(f.restart().read(scope), saved)
  for (const field of ['networkId', 'deviceId', 'conversationId', 'inputTarget']) {
    assert.equal(f.repository.read({ ...scope, [field]: 'other' }).text, '')
    assert.notEqual(draftScopeKey({ ...scope, [field]: 'other' }), saved.key)
  }
  assert.throws(() => f.repository.read({ ...scope, networkId: '' }))
})

test('text edits reuse owned files, and leases preserve removed files until the last reader finishes', async () => {
  const f = fixture()
  const first = await f.repository.write(scope, { text: 'one', files: [selected] })
  const one = f.repository.lease(first), two = f.repository.lease(first)
  const next = await f.repository.write(scope, { text: 'two', files: first.files })
  assert.equal(f.count(), 1)
  assert.equal(await f.repository.clear(scope, first), false)
  assert.equal(await f.repository.clear(scope, next), true)
  assert.equal(f.copies.size, 1)
  one.release(); one.release()
  assert.equal(f.copies.size, 1)
  two.release()
  assert.deepEqual(f.released, [first.files[0].uri])
  assert.equal(f.repository.read(scope).text, '')
})

test('same-scope writes serialize through a slow retain; a saved send cannot clear a newer edit', async () => {
  const f = fixture()
  let finish = () => {}
  f.delay(new Promise<void>(resolve => { finish = resolve }))
  const first = f.repository.write(scope, { text: 'first', files: [selected] })
  const next = f.repository.write(scope, { text: 'newer', files: [] })
  assert.equal(f.repository.read(scope).text, '')
  finish()
  const snapshot = await first
  const updated = await next
  assert.equal(f.repository.read(scope).text, 'newer')
  assert.equal(await f.repository.clear(scope, snapshot), false)
  assert.equal(f.repository.read(scope).revision, updated.revision)
  assert.equal(f.copies.size, 0)
})

test('failed commit preserves the complete old draft and releases only newly copied files', async () => {
  const f = fixture()
  const saved = await f.repository.write(scope, { text: 'old', files: [selected] })
  f.fail(true)
  await assert.rejects(f.repository.write(scope, { text: 'new', files: [selected] }), /disk full/)
  assert.deepEqual(f.repository.read(scope), saved)
  assert.equal(f.copies.size, 1)
  assert.equal(f.released.length, 1)
  f.fail(false)
  assert.equal((await f.repository.write(scope, { text: 'retry', files: saved.files })).text, 'retry')
})

test('system-cleared attachments recover text and report the specific re-selection needed', async () => {
  const f = fixture()
  const saved = await f.repository.write(scope, { text: 'keep text', files: [selected] })
  f.copies.clear()
  const restored = f.restart().read(scope)
  assert.equal(restored.text, 'keep text')
  assert.deepEqual(restored.files, [])
  assert.deepEqual(restored.missingFiles, [selected.name])
  assert.throws(() => f.repository.lease(saved), /重新选择/)
  await assert.rejects(f.repository.write(scope, saved), /重新选择/)
})

test('a foreign-scope leased attachment receives an independent private copy', async () => {
  const f = fixture()
  const saved = await f.repository.write(scope, { text: '', files: [selected] })
  const lease = f.repository.lease(saved)
  const foreign = await f.repository.write({ ...scope, networkId: 'other' }, saved)
  assert.notEqual(foreign.files[0].uri, saved.files[0].uri)
  await f.repository.clear(scope, saved)
  lease.release()
  assert.equal(f.copies.has(foreign.files[0].uri), true)
})
