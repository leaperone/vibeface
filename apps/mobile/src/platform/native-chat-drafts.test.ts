import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { moduleLoader } from '../../../app/src/lib/test-harness.ts'
import { createChatDrafts, type DraftScope } from './chat-drafts.ts'
import type { chatDrafts } from './native-chat-drafts.ts'

/** Exercise the native ownership adapter in the existing VM harness; only the unavailable native filesystem is substituted. */
test('queued writes retain picker ownership until every queued native copy completes', async () => {
  const contents = new Map<string, number>([['picker:report', 3]])
  const storage = new Map<string, string>()
  const released: string[] = []
  let sequence = 0, unblock = () => {}
  const gate = new Promise<void>(resolve => { unblock = resolve })
  class File {
    uri: string
    constructor(parent: string | { uri: string }, name?: string) { this.uri = typeof parent === 'string' ? parent : `${parent.uri}/${name}` }
    get exists() { return contents.has(this.uri) }
    get size() { return contents.get(this.uri) ?? 0 }
    async copy(destination: File) { await gate; assert(this.exists, 'picker source must survive all queued readers'); contents.set(destination.uri, this.size) }
    delete() { contents.delete(this.uri) }
  }
  const directory = { uri: 'private:drafts', exists: false, create() {}, list: () => [] }
  const load = moduleLoader(createContext({ crypto: { randomUUID: () => `copy-${++sequence}` } }), specifier => {
    if (specifier === 'expo-file-system') return { File }
    if (specifier === './chat-drafts') return { createChatDrafts }
    if (specifier === './files') return { privateFiles: {
      drafts: directory,
      draftStorage: { get: (key: string) => storage.get(key), set: (key: string, value: string) => storage.set(key, value), remove: (key: string) => storage.delete(key), all: () => [...storage.values()] },
      run: async (operation: (current: () => boolean) => Promise<unknown>) => operation(() => true),
      release(uri: string) { if (uri.startsWith('picker:')) { released.push(uri); contents.delete(uri) } },
    } }
    throw new Error(`Unexpected dependency ${specifier}`)
  })
  const loaded = load(fileURLToPath(new URL('./native-chat-drafts.ts', import.meta.url))).chatDrafts
  assert(loaded && typeof loaded === 'object' && 'write' in loaded && typeof loaded.write === 'function')
  const write = loaded.write
  const call = (scope: DraftScope, text: string): ReturnType<typeof chatDrafts.write> => Reflect.apply(write, loaded, [scope, { text, files: [{ uri: 'picker:report', name: 'report.txt', size: 3, mimeType: 'text/plain' }] }])
  const scope = { networkId: 'n', deviceId: 'd', conversationId: 'c', inputTarget: 'main' }
  const first = call(scope, 'first'), latest = call(scope, 'latest')
  assert.deepEqual(released, [])
  unblock()
  const old = await first, current = await latest
  assert.equal(current.text, 'latest')
  assert.notEqual(old.files[0].uri, current.files[0].uri)
  assert.equal(contents.has(current.files[0].uri), true)
  assert.deepEqual(released, ['picker:report'])
})

function deferred() {
  let resolve = () => {}
  const promise = new Promise<void>(done => { resolve = done })
  return { promise, resolve }
}

function readerFixture() {
  const contents = new Map<string, Uint8Array>([['picker:report', new Uint8Array([7, 8, 9])]])
  const storage = new Map<string, string>(), readUris: string[] = []
  const reading = deferred(), finishRead = deferred()
  let sequence = 0, operations = 0
  class File {
    uri: string
    constructor(parent: string | { uri: string }, name?: string) { this.uri = typeof parent === 'string' ? parent : `${parent.uri}/${name}` }
    get exists() { return contents.has(this.uri) }
    get size() { return contents.get(this.uri)?.byteLength ?? 0 }
    async copy(destination: File) {
      const bytes = contents.get(this.uri)
      assert(bytes)
      contents.set(destination.uri, bytes.slice())
    }
    async arrayBuffer() {
      readUris.push(this.uri)
      reading.resolve()
      await finishRead.promise
      const bytes = contents.get(this.uri)
      assert(bytes, 'an in-flight reader must keep the retired durable file alive')
      return bytes.slice().buffer
    }
    delete() { contents.delete(this.uri) }
  }
  const load = moduleLoader(createContext({ crypto: { randomUUID: () => `read-${++sequence}` } }), specifier => {
    if (specifier === 'expo-file-system') return { File }
    if (specifier === './chat-drafts') return { createChatDrafts }
    if (specifier === './files') return { privateFiles: {
      drafts: { uri: 'private:drafts', exists: false, create() {}, list: () => [] },
      draftStorage: { get: (key: string) => storage.get(key), set: (key: string, value: string) => { storage.set(key, value) }, remove: (key: string) => { storage.delete(key) }, all: () => [...storage.values()] },
      run: async (operation: (current: () => boolean) => Promise<unknown>) => { operations++; return operation(() => true) },
      release: (uri: string) => { contents.delete(uri) },
    } }
    throw new Error(`Unexpected dependency ${specifier}`)
  })
  const native = load(fileURLToPath(new URL('./native-chat-drafts.ts', import.meta.url))).chatDrafts
  assert(native && typeof native === 'object')
  const repository = {
    write(...args: Parameters<typeof chatDrafts.write>): ReturnType<typeof chatDrafts.write> { return Reflect.apply(Reflect.get(native, 'write'), native, args) },
    read(...args: Parameters<typeof chatDrafts.read>): ReturnType<typeof chatDrafts.read> { return Reflect.apply(Reflect.get(native, 'read'), native, args) },
    clear(...args: Parameters<typeof chatDrafts.clear>): ReturnType<typeof chatDrafts.clear> { return Reflect.apply(Reflect.get(native, 'clear'), native, args) },
    lease(...args: Parameters<typeof chatDrafts.lease>): ReturnType<typeof chatDrafts.lease> { return Reflect.apply(Reflect.get(native, 'lease'), native, args) },
  }
  return { repository, contents, storage, readUris, reading, finishRead, operations: () => operations }
}

test('native writes and restored drafts carry only durable metadata, and leased readers use that URI', async () => {
  const f = readerFixture()
  const scope = { networkId: 'n', deviceId: 'd', conversationId: 'c', inputTarget: 'main' }
  let oldReads = 0, oldDisposals = 0
  const picked = {
    uri: 'picker:report', name: 'report.txt', size: 3, mimeType: 'text/plain',
    async arrayBuffer() { oldReads++; throw new Error('stale picker reader') },
    dispose() { oldDisposals++ },
  }
  const saved = await f.repository.write(scope, { text: 'keep', files: [picked] })
  const durable = saved.files[0]
  assert.notEqual(durable.uri, picked.uri)
  assert.equal(f.contents.has(picked.uri), true, 'owned selection cleanup belongs to its staging owner')
  assert.deepEqual(Object.keys(durable).sort(), ['mimeType', 'name', 'size', 'uri'])
  // Existing durable inputs can also carry stale capabilities; reuse must normalize them too.
  const reused = await f.repository.write(scope, { text: 'edit', files: [{ ...durable, ...{ arrayBuffer: picked.arrayBuffer, dispose: picked.dispose } }] })
  assert.equal(reused.files[0].uri, durable.uri)
  assert.deepEqual(Object.keys(reused.files[0]).sort(), ['mimeType', 'name', 'size', 'uri'])
  const restored = f.repository.read(scope)
  assert.deepEqual(Object.keys(restored.files[0]).sort(), ['mimeType', 'name', 'size', 'uri'])
  const lease = f.repository.lease(restored)
  const before = f.operations()
  const bytes = lease.files[0].arrayBuffer()
  await f.reading.promise
  assert.equal(f.operations(), before + 1, 'native readers participate in privateFiles.run')
  assert.deepEqual(f.readUris, [durable.uri])
  f.finishRead.resolve()
  assert.deepEqual([...new Uint8Array(await bytes)], [7, 8, 9])
  lease.release()
  assert.equal(oldReads, 0)
  assert.equal(oldDisposals, 0)
})

test('an active native reader outlives draft clear and outer lease release, then retires its copy', async () => {
  const f = readerFixture()
  const scope = { networkId: 'n', deviceId: 'd', conversationId: 'c', inputTarget: 'main' }
  const saved = await f.repository.write(scope, { text: '', files: [{ uri: 'picker:report', name: 'report.txt', size: 3, mimeType: null }] })
  const lease = f.repository.lease(saved), uri = saved.files[0].uri
  const bytes = lease.files[0].arrayBuffer()
  await f.reading.promise
  assert.equal(await f.repository.clear(scope, saved), true)
  lease.release()
  lease.release()
  assert.equal(f.contents.has(uri), true)
  const operations = f.operations()
  await assert.rejects(lease.files[0].arrayBuffer(), /读取已结束/)
  assert.equal(f.operations(), operations, 'released readers cannot enter a new private-files operation')
  assert.deepEqual(f.readUris, [uri])
  f.finishRead.resolve()
  assert.deepEqual([...new Uint8Array(await bytes)], [7, 8, 9])
  assert.equal(f.contents.has(uri), false)
  assert.equal(f.repository.read(scope).files.length, 0)
  await assert.rejects(lease.files[0].arrayBuffer(), /读取已结束/)
})
