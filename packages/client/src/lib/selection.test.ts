import assert from 'node:assert/strict'
import test from 'node:test'
import { selectionOwner } from './selection.ts'
import { enqueue } from './outbox.ts'

function deferred() {
  let resolve = () => {}, reject = (_cause: unknown) => {}
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
function fixture() {
  const released: string[] = [], attempts: string[] = [], failed = new Set<string>()
  const file = (name: string) => ({ name, dispose() { attempts.push(name); if (failed.has(name)) throw new Error(name); released.push(name) } })
  return { released, attempts, failed, file }
}

test('identity diffs deduplicate, preserve overlap, and cannot resurrect a released selection', () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), b = f.file('B'), c = f.file('C')
  owner.replace([a, a, b]); owner.replace([b, c]); owner.replace([b, c])
  assert.deepEqual(f.released, ['A'])
  owner.replace([]); owner.close(); owner.close()
  assert.deepEqual(f.released, ['A', 'B', 'C'])
  assert.equal(owner.replace([a]), false)
  assert.deepEqual(f.released, ['A', 'B', 'C'])
})

test('a queued operation borrows every file before its first read, through durable enqueue', async () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), b = f.file('B'), queue = deferred(), put = deferred()
  owner.replace([a, b])
  const stored: unknown[] = []
  const saving = owner.use(async files => {
    await queue.promise
    assert.deepEqual(f.released, [])
    const uploads = files.map(file => ({ hash: file.name, size: 1, name: file.name, data: 'YQ==' }))
    await enqueue({ all: async () => [], put: async item => { await put.promise; stored.push(item) }, delete: async () => {} }, { key: 'message:A', method: 'message', scope_id: 'c', delivery: 'saved', params: { conversation_id: 'c', message_id: 'A', text: 'a', report_results: true, target: { kind: 'bot_main', bot_id: 'b' }, intent: 'continue', sequence: 1, attestation: { device_id: 'd', signature: 's' } }, created_at_ms: 1, uploads })
    return uploads
  })
  owner.replace([]); owner.close()
  assert.deepEqual(f.released, [])
  queue.resolve(); await Promise.resolve(); await Promise.resolve()
  assert.deepEqual(f.released, [])
  put.resolve()
  const uploads = await saving
  assert.deepEqual(f.released, ['A', 'B'])
  assert.deepEqual((stored[0] as { uploads: unknown }).uploads, uploads)
})

test('concurrent operations defer release until both success and rejection have settled', async () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), first = deferred(), second = deferred()
  owner.replace([a])
  const one = owner.use(async () => first.promise), two = owner.use(async () => second.promise)
  const rejected = assert.rejects(two, /failed/)
  owner.replace([]); first.resolve(); await one
  assert.deepEqual(f.released, [])
  second.reject(new Error('failed')); await rejected
  assert.deepEqual(f.released, ['A'])
  owner.close()
  assert.deepEqual(f.released, ['A'])
})

test('failed saving retains retry selection; success can consume only its own UI draft', async () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), b = f.file('B'), gate = deferred()
  owner.replace([a])
  await assert.rejects(owner.use(async () => { throw new Error('put failed') }), /put failed/)
  assert.deepEqual(f.released, [])
  const oldSave = owner.use(async files => { await gate.promise; return files })
  owner.replace([a, b]); gate.resolve(); assert.deepEqual(await oldSave, [a])
  assert.deepEqual(f.released, [])
  owner.replace([b]); assert.deepEqual(f.released, ['A'])
  owner.close(); assert.deepEqual(f.released, ['A', 'B'])
})

test('retired but borrowed files can be adopted again before disposal', async () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), gate = deferred()
  owner.replace([a]); const saving = owner.use(async () => gate.promise)
  owner.replace([]); owner.replace([a]); gate.resolve(); await saving
  assert.deepEqual(f.released, [])
  owner.replace([]); assert.deepEqual(f.released, ['A'])
  assert.throws(() => owner.replace([a]), /重新选择/)
})

test('cleanup attempts all objects and retains failures for explicit close retry', () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), b = f.file('B'); f.failed.add('A')
  owner.replace([a, b])
  assert.throws(() => owner.replace([]), AggregateError)
  assert.deepEqual(f.attempts, ['A', 'B']); assert.deepEqual(f.released, ['B'])
  assert.throws(() => owner.replace([a]), /重新选择/, 'a failed disposal cannot revive a retired native handle')
  f.failed.clear(); owner.close(); owner.close()
  assert.deepEqual(f.attempts, ['A', 'B', 'A']); assert.deepEqual(f.released, ['B', 'A'])
})

test('closed owner rejects late handoffs, cleans all candidates once, and rejects empty use', async () => {
  const f = fixture(), owner = selectionOwner<ReturnType<typeof f.file>>(error => { throw error })
  const a = f.file('A'), b = f.file('B')
  owner.close(); assert.equal(owner.replace([a, b]), false); owner.replace([a]); owner.close()
  assert.deepEqual(f.released, ['A', 'B'])
  await assert.rejects(owner.use(async () => null), /选择文件/)
})

test('web selections with no dispose remain valid operation inputs', async () => {
  const owner = selectionOwner<{ name: string; dispose?(): void | Promise<void> }>(error => { throw error }), file = { name: 'browser original' }
  owner.replace([file]); assert.equal(await owner.use(async ([picked]) => picked.name), file.name)
  owner.replace([]); owner.close()
})
