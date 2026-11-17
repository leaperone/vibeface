import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { moduleLoader } from '../../../app/src/lib/test-harness.ts'
import * as validation from './chat-attachments.ts'
import { sha256Hex } from '@vibeface/client/lib/byte-codec.ts'
import type * as cache from './native-chat-attachments.ts'

function deferred() {
  let resolve = () => {}
  const promise = new Promise<void>(yes => { resolve = yes })
  return { promise, resolve }
}
function fixture() {
  const root = 'file:///app/chat-attachments', content = new Map<string, Uint8Array>(), directories = new Set<string>(), deleted: string[] = []
  const scope = '1'.repeat(64)
  let writes: Promise<void> | undefined, shares: Promise<void> | undefined, reads: Promise<void> | undefined, eraseEpoch = 0, erasing = false, sequence = 0, nativeShares = 0, deletionFailure = false, listing: ((uri: string) => (Directory | File)[]) | undefined
  const globalPending = new Set<Promise<unknown>>(), shareEntered = deferred(), writeEntered = deferred(), readEntered = deferred()
  const uriOf = (parts: (string | { uri: string })[]) => parts.map(part => typeof part === 'string' ? part : part.uri).map(part => part.replace(/\/$/, '')).join('/')
  class File {
    uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(parts) }
    get exists() { return content.has(this.uri) }
    get size() { return content.get(this.uri)?.length ?? 0 }
    async bytes() { readEntered.resolve(); await reads; const value = content.get(this.uri); if (!value) throw new Error('file missing'); return value }
    async write(bytes: Uint8Array) { writeEntered.resolve(); await writes; content.set(this.uri, bytes) }
    async move(destination: File) { const bytes = content.get(this.uri); assert(bytes); content.set(destination.uri, bytes); content.delete(this.uri); this.uri = destination.uri }
    delete() { content.delete(this.uri) }
  }
  class Directory {
    uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(parts) }
    get exists() { return directories.has(this.uri) }
    create() { let path = this.uri; while (path.startsWith(root)) { directories.add(path); path = path.slice(0, path.lastIndexOf('/')) } }
    list(): (Directory | File)[] {
      if (listing) return listing(this.uri)
      if (!this.exists) throw new Error('directory missing')
      const prefix = `${this.uri}/`
      return [
        ...[...directories].filter(uri => uri.startsWith(prefix) && !uri.slice(prefix.length).includes('/')).map(uri => new Directory(uri)),
        ...[...content.keys()].filter(uri => uri.startsWith(prefix) && !uri.slice(prefix.length).includes('/')).map(uri => new File(uri)),
      ]
    }
    delete() { if (deletionFailure) throw new Error('native delete failed'); deleted.push(this.uri); for (const uri of directories) if (uri === this.uri || uri.startsWith(`${this.uri}/`)) directories.delete(uri); for (const uri of content.keys()) if (uri.startsWith(`${this.uri}/`)) content.delete(uri) }
  }
  const attachments = new Directory(root)
  async function run<T>(operation: (current: () => boolean) => Promise<T>) {
    if (erasing) throw new Error('global erase')
    const owner = eraseEpoch, current = () => !erasing && owner === eraseEpoch
    const work = operation(current)
    globalPending.add(work)
    try { const value = await work; if (!current()) throw new Error('global erase'); return value } finally { globalPending.delete(work) }
  }
  const load = moduleLoader(createContext({ crypto: { randomUUID: () => `temporary-${++sequence}` }, Promise, Set, Number, Error, decodeURIComponent }), specifier => {
    if (specifier === 'expo-file-system') return { File, Directory }
    if (specifier === 'expo-sharing') return { isAvailableAsync: async () => true, shareAsync: async () => { nativeShares++; shareEntered.resolve(); await shares } }
    if (specifier === './chat-attachments') return validation
    if (specifier === './files') return { privateFiles: { attachments, run } }
    throw new Error(`Unexpected dependency ${specifier}`)
  })
  const api = load(fileURLToPath(new URL('./native-chat-attachments.ts', import.meta.url)))
  function invoke(name: keyof typeof cache, args: unknown[]): unknown {
    const value = api[name]
    assert(typeof value === 'function')
    return Reflect.apply(value, api, args)
  }
  const methods = {
    async localAttachment(...args: Parameters<typeof cache.localAttachment>): Promise<cache.LocalAttachment> {
      const value = await invoke('localAttachment', args)
      assert(value && typeof value === 'object' && 'uri' in value && typeof value.uri === 'string' && 'mimeType' in value && typeof value.mimeType === 'string' && 'image' in value && typeof value.image === 'boolean')
      return { uri: value.uri, mimeType: value.mimeType, image: value.image }
    },
    attachmentCacheStats() {
      const value = invoke('attachmentCacheStats', [])
      assert(value && typeof value === 'object' && 'bytes' in value && typeof value.bytes === 'number' && 'files' in value && typeof value.files === 'number')
      return { bytes: value.bytes, files: value.files }
    },
    async clearAttachmentCache() { await invoke('clearAttachmentCache', []) },
    async shareAttachment(...args: Parameters<typeof cache.shareAttachment>) { await invoke('shareAttachment', args) },
  }

  const bytes = new TextEncoder().encode('verified cache content')
  async function file() { return { hash: await sha256Hex(bytes), size: bytes.length, name: 'report.txt' } }
  return {
    ...methods, readEntered: readEntered.promise, writeEntered: writeEntered.promise, shareEntered: shareEntered.promise, scope, bytes, file, content, directories, deleted, root, File, Directory,
    failDeletion(value: boolean) { deletionFailure = value }, writes(value: Promise<void> | undefined) { writes = value }, reads(value: Promise<void> | undefined) { reads = value }, shares(value: Promise<void> | undefined) { shares = value }, listing(value: typeof listing) { listing = value }, nativeShares: () => nativeShares,
    async globalErase() { erasing = true; eraseEpoch++; try { await Promise.allSettled([...globalPending]); if (attachments.exists) attachments.delete() } finally { erasing = false } },
  }
}

test('statistics count only fixed-layout private media; cleanup preserves all other files and allows later downloads', async () => {
  const f = fixture(), file = await f.file()
  assert.deepEqual({ ...f.attachmentCacheStats() }, { bytes: 0, files: 0 })
  const local = await f.localAttachment(f.scope, file, async () => ({ bytes: f.bytes, size: file.size, next_offset: null }))
  f.content.set('file:///app/chat-drafts/keep', f.bytes)
  f.content.set('file:///app/recovery/keep', f.bytes)
  assert.deepEqual({ ...f.attachmentCacheStats() }, { bytes: file.size, files: 1 })
  assert.equal((await f.localAttachment(f.scope, file)).uri, local.uri)
  await f.clearAttachmentCache()
  assert.deepEqual(f.deleted, [f.root])
  assert.equal(f.content.has('file:///app/chat-drafts/keep'), true)
  assert.equal(f.content.has('file:///app/recovery/keep'), true)
  assert.deepEqual({ ...f.attachmentCacheStats() }, { bytes: 0, files: 0 })
  await assert.rejects(f.localAttachment(f.scope, file), /尚未下载/)
  await f.localAttachment(f.scope, file, async () => ({ bytes: f.bytes, size: file.size, next_offset: null }))
  assert.equal(f.attachmentCacheStats().files, 1)
})

test('clearing invalidates an in-flight write, rejects new media operations and waits before deleting', async () => {
  const f = fixture(), file = await f.file(), gate = deferred()
  f.writes(gate.promise)
  const downloading = f.localAttachment(f.scope, file, async () => ({ bytes: f.bytes, size: file.size, next_offset: null }))
  const rejected = assert.rejects(downloading, /清除/)
  await f.writeEntered
  const clearing = f.clearAttachmentCache()
  assert.throws(() => f.attachmentCacheStats(), /清除/)
  await assert.rejects(f.localAttachment(f.scope, file), /清除/)
  await assert.rejects(f.clearAttachmentCache(), /清除/)
  assert.deepEqual(f.deleted, [])
  gate.resolve()
  await rejected; await clearing
  assert.equal(f.content.size, 0)
  assert.equal(f.attachmentCacheStats().files, 0)
})

test('clearing waits for native share completion and rejects stale share results', async () => {
  const f = fixture(), file = await f.file(), gate = deferred()
  const local = await f.localAttachment(f.scope, file, async () => ({ bytes: f.bytes, size: file.size, next_offset: null }))
  f.shares(gate.promise)
  const sharing = f.shareAttachment(local, 'share'), rejected = assert.rejects(sharing, /清除/)
  await f.shareEntered
  assert.equal(f.nativeShares(), 1)
  const clearing = f.clearAttachmentCache()
  assert.deepEqual(f.deleted, [])
  gate.resolve()
  await rejected; await clearing
  assert.deepEqual(f.deleted, [f.root])
})

test('global erase overlapping media clear drains both without recreating cache or a dependency cycle', async () => {
  const f = fixture(), file = await f.file(), gate = deferred(), entered = deferred()
  const downloading = f.localAttachment(f.scope, file, async () => { entered.resolve(); await gate.promise; return { bytes: f.bytes, size: file.size, next_offset: null } })
  const rejected = assert.rejects(downloading, /清除|global erase/)
  await entered.promise
  const clearing = f.clearAttachmentCache(), clearRejected = assert.rejects(clearing, /清除|global erase/)
  const erasing = f.globalErase()
  gate.resolve()
  await Promise.all([rejected, clearRejected, erasing])
  assert.equal(f.content.size, 0)
  assert.equal(f.attachmentCacheStats().files, 0)
})

test('statistics reject escaped child URIs, unknown directories and excessive nesting', () => {
  const f = fixture()
  f.directories.add(f.root)
  f.listing(() => [new f.Directory('file:///app/chat-drafts')])
  assert.throws(() => f.attachmentCacheStats(), /超出/)
  f.listing(uri => [new f.Directory(uri, 'unknown')])
  assert.throws(() => f.attachmentCacheStats(), /无法识别/)
  f.listing(uri => [new f.Directory(uri, '1'.repeat(64))])
  assert.throws(() => f.attachmentCacheStats(), /无法识别/)
})


test('failed native deletion reports failure and restores access for a later explicit retry', async () => {
  const f = fixture(), file = await f.file()
  await f.localAttachment(f.scope, file, async () => ({ bytes: f.bytes, size: file.size, next_offset: null }))
  f.failDeletion(true)
  await assert.rejects(f.clearAttachmentCache(), /native delete failed/)
  assert.equal(f.attachmentCacheStats().files, 1)
  f.failDeletion(false)
  await f.clearAttachmentCache()
  assert.equal(f.attachmentCacheStats().files, 0)
})

test('cache clear rejects a late cached-read result, and never shares a file outside the media root', async () => {
  const f = fixture(), file = await f.file(), gate = deferred()
  const local = await f.localAttachment(f.scope, file, async () => ({ bytes: f.bytes, size: file.size, next_offset: null }))
  f.reads(gate.promise)
  const reading = f.localAttachment(f.scope, file), rejected = assert.rejects(reading, /清除/)
  const clearing = f.clearAttachmentCache()
  gate.resolve()
  await rejected; await clearing
  await assert.rejects(f.shareAttachment({ ...local, uri: 'file:///app/chat-drafts/keep' }, 'share'), /不属于/)
  assert.equal(f.nativeShares(), 0)
})
