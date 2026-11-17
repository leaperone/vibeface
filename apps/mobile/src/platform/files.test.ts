import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { selectionOwner } from '../../../../packages/client/src/lib/selection.ts'
import { moduleLoader } from '../../../app/src/lib/test-harness.ts'

function deferred() {
  let resolve = (_value: void) => {}, reject = (_reason: unknown) => {}
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const tick = () => new Promise<void>(resolve => setImmediate(resolve))

/** Only the native filesystem, private index and UUID boundary are replaced. */
function fixture() {
  const bytes = new Map<string, Uint8Array>()
  const directories = new Set(['file:///cache', 'file:///documents'])
  const index = new Map<string, string>()
  const events: string[] = []
  const copies: ({ source: string; target: File } & ReturnType<typeof deferred>)[] = []
  const failedDeletes = new Set<string>()
  const reads: ({ uri: string; kind: string } & ReturnType<typeof deferred>)[] = []
  let uuid = 0
  const uriOf = (...parts: (string | { uri: string })[]) => parts.map(part => typeof part === 'string' ? part : part.uri).join('/').replace(/\/$/, '')
  class Directory {
    readonly uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(...parts) }
    get exists() { return directories.has(this.uri) }
    create() { directories.add(this.uri) }
    list() { return [...bytes.keys()].filter(uri => uri.startsWith(this.uri + '/') && !uri.slice(this.uri.length + 1).includes('/')).map(uri => new File(uri)) }
    delete() {
      events.push(`directory:delete:${this.uri}`)
      for (const uri of bytes.keys()) if (uri.startsWith(this.uri + '/')) bytes.delete(uri)
      directories.delete(this.uri)
    }
  }
  class File {
    readonly uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(...parts) }
    get exists() { return bytes.has(this.uri) }
    get name() { return this.uri.split('/').at(-1) ?? '' }
    get size() { return bytes.get(this.uri)?.length ?? 0 }
    copySync(): never { assert.fail('attachment retain must never call copySync') }
    async copy(target: File): Promise<void> {
      const gate = deferred()
      copies.push({ source: this.uri, target, ...gate })
      events.push(`copy:start:${this.uri}:${target.uri}`)
      await gate.promise
      const original = bytes.get(this.uri)
      assert.ok(original)
      bytes.set(target.uri, original.slice())
      events.push(`copy:done:${target.uri}`)
      // Expo's SDK resolves undefined, not a File.
    }
    async text() {
      const gate = deferred(); reads.push({ uri: this.uri, kind: 'text', ...gate })
      await gate.promise
      return new TextDecoder().decode(bytes.get(this.uri))
    }
    async arrayBuffer(): Promise<ArrayBuffer> {
      const gate = deferred(); reads.push({ uri: this.uri, kind: 'arrayBuffer', ...gate })
      await gate.promise
      return bytes.get(this.uri)!.slice().buffer as ArrayBuffer
    }
    delete() {
      events.push(`file:delete:${this.uri}`)
      if (failedDeletes.has(this.uri)) throw new Error('controlled delete failure')
      bytes.delete(this.uri)
    }
  }
  const load = moduleLoader(createContext({
    crypto: { randomUUID: () => `owned-${++uuid}` }, AggregateError,
  }), specifier => {
    if (specifier === 'expo-file-system') return { File, Directory, Paths: { cache: new Directory('file:///cache'), document: new Directory('file:///documents') } }
    if (specifier === 'react-native-mmkv') return { createMMKV: () => ({
      getString: (key: string) => index.get(key), getAllKeys: () => [...index.keys()],
      set: (key: string, value: string) => index.set(key, value), remove: (key: string) => index.delete(key),
      clearAll() { events.push('index:clear'); index.clear() },
    }) }
    if (specifier === '@vibeface/client/lib/node.ts') return { record: (value: unknown) => value }
    throw new Error(`Unexpected dependency: ${specifier}`)
  })
  const { privateFiles } = load(fileURLToPath(new URL('./files.ts', import.meta.url))) as typeof import('./files.ts')
  return { privateFiles, File, bytes, copies, events, failedDeletes, reads, directories }
}

test('retain waits for SDK copy resolving undefined and returns its own target once', async () => {
  const f = fixture(), source = 'content://provider/original.pdf'
  f.bytes.set(source, new Uint8Array([1, 2, 3]))
  let deliveries = 0
  const work = f.privateFiles.run(async current => {
    const target = await f.privateFiles.retain(source)
    assert.equal(current(), true)
    deliveries++
    return target
  })
  await tick()
  assert.equal(deliveries, 0)
  assert.equal(f.copies.length, 1)
  assert.deepEqual(f.events, [`copy:start:${source}:file:///cache/vibeface-selections/owned-1`])
  f.copies[0].resolve(undefined)
  const target = await work
  assert.equal(target, f.copies[0].target)
  assert.ok(target instanceof f.File)
  assert.equal(deliveries, 1)
  assert.equal(target.uri, 'file:///cache/vibeface-selections/owned-1')
  assert.deepEqual(f.bytes.get(target.uri), new Uint8Array([1, 2, 3]))
  assert.deepEqual(f.bytes.get(source), new Uint8Array([1, 2, 3]))
  assert.deepEqual(f.events, [
    `copy:start:${source}:file:///cache/vibeface-selections/owned-1`,
    'copy:done:file:///cache/vibeface-selections/owned-1',
  ])
})

test('a partial copy rejection cleans only its target and retains both errors if deletion fails', async (t) => {
  for (const deleteFails of [false, true]) await t.test(`delete failure ${deleteFails}`, async () => {
    const f = fixture(), source = 'ph://photo-original', provider = 'content://provider/original'
    f.bytes.set(source, new Uint8Array([4, 5, 6])); f.bytes.set(provider, new Uint8Array([7]))
    const failure = new Error('controlled partial copy failure')
    const work = f.privateFiles.run(() => f.privateFiles.retain(source))
    const rejected = assert.rejects(work, cause => {
      if (!deleteFails) assert.equal(cause, failure)
      else {
        assert.ok(cause instanceof AggregateError)
        assert.equal(cause.errors[0], failure)
        assert.match(cause.errors[1].message, /controlled delete failure/)
      }
      return true
    })
    const target = f.copies[0].target
    f.bytes.set(target.uri, new Uint8Array([4]))
    if (deleteFails) f.failedDeletes.add(target.uri)
    f.copies[0].reject(failure)
    await rejected
    assert.equal(target.exists, deleteFails)
    f.privateFiles.release(source); f.privateFiles.release(provider)
    f.privateFiles.releasePicker(source); f.privateFiles.releasePicker(provider)
    assert.deepEqual(f.bytes.get(source), new Uint8Array([4, 5, 6]))
    assert.deepEqual(f.bytes.get(provider), new Uint8Array([7]))
    assert.deepEqual(f.events, [`copy:start:${source}:${target.uri}`, `file:delete:${target.uri}`])
  })
})

test('erase fences pending copy delivery, rejects new work, and clears only after SDK copy settles', async () => {
  const f = fixture(), source = 'content://provider/original', picker = 'file:///cache/DocumentPicker/cache.pdf'
  f.bytes.set(source, new Uint8Array([8, 9])); f.bytes.set(picker, new Uint8Array([8, 9]))
  let deliveries = 0, clears = 0
  const work = f.privateFiles.run(async current => {
    const target = await f.privateFiles.retain(picker)
    try {
      if (!current()) { f.privateFiles.release(target.uri); return null }
      deliveries++
      return target
    } finally { f.privateFiles.releasePicker(picker) }
  })
  const rejected = assert.rejects(work, /正在清除/)
  const erase = f.privateFiles.erase(async () => { clears++; f.events.push('clear') })
  await tick()
  await assert.rejects(f.privateFiles.run(async () => { assert.fail('erasing run must not start') }), /正在清除/)
  await assert.rejects(f.privateFiles.retain(source), /正在清除/)
  assert.equal(f.copies.length, 1)
  assert.equal(deliveries, 0)
  assert.equal(clears, 0)
  assert.deepEqual(f.events, [`copy:start:${picker}:file:///cache/vibeface-selections/owned-1`])
  f.copies[0].resolve(undefined)
  await Promise.all([erase, rejected])
  assert.equal(deliveries, 0)
  assert.equal(clears, 1)
  assert.equal(f.bytes.has(picker), false)
  assert.equal(f.copies[0].target.exists, false)
  assert.deepEqual(f.bytes.get(source), new Uint8Array([8, 9]))
  assert.deepEqual(f.events, [
    `copy:start:${picker}:file:///cache/vibeface-selections/owned-1`,
    'copy:done:file:///cache/vibeface-selections/owned-1',
    'file:delete:file:///cache/vibeface-selections/owned-1',
    `file:delete:${picker}`, 'directory:delete:file:///cache/vibeface-selections', 'index:clear', 'clear',
  ])
  await f.privateFiles.run(async current => { assert.equal(current(), true) })
})


for (const kind of ['text', 'arrayBuffer'] as const) for (const fails of [false, true]) test(`owned ${kind} reader defers idempotent disposal, rejection ${fails}`, async () => {
  const f = fixture(), uri = 'file:///cache/vibeface-selections/reader'
  f.bytes.set(uri, new TextEncoder().encode('original bytes'))
  const owned = f.privateFiles.ownedFile(new f.File(uri))
  const read = owned[kind]()
  const result = fails ? assert.rejects(read, /controlled read failure/) : read
  const disposal = owned.dispose()
  assert.equal(owned.dispose(), disposal)
  assert.equal(f.bytes.has(uri), true)
  await assert.rejects(owned[kind](), /重新选择/)
  assert.equal(f.reads.length, 1)
  if (fails) f.reads[0].reject(new Error('controlled read failure'))
  else f.reads[0].resolve()
  const value = await result
  await disposal
  if (!fails) assert.equal(kind === 'text' ? value : new TextDecoder().decode(value as ArrayBuffer), 'original bytes')
  assert.equal(f.bytes.has(uri), false)
  owned.dispose()
  assert.deepEqual(f.events, [`file:delete:${uri}`])
})

for (const kind of ['text', 'arrayBuffer'] as const) test(`erase waits for an owned ${kind} reader and permanently invalidates its epoch`, async () => {
  const f = fixture(), uri = 'file:///cache/vibeface-selections/reader', original = 'content://original'
  f.bytes.set(uri, new Uint8Array([97])); f.bytes.set(original, new Uint8Array([98]))
  f.directories.add('file:///cache/vibeface-selections')
  const owned = f.privateFiles.ownedFile(new f.File(uri))
  const read = owned[kind](), rejected = assert.rejects(read, /正在清除/)
  let cleared = false
  const erase = f.privateFiles.erase(async () => { cleared = true })
  await tick()
  assert.equal(cleared, false); assert.equal(f.bytes.has(uri), true)
  assert.equal(f.events.some(event => event.startsWith('directory:delete')), false)
  await assert.rejects(owned[kind](), /重新选择/)
  f.reads[0].resolve(); await rejected; await erase
  assert.equal(cleared, true); assert.equal(f.bytes.has(original), true)
  await assert.rejects(owned[kind](), /重新选择/)
  owned.dispose(); owned.dispose()
  assert.equal(f.events.filter(event => event === `file:delete:${uri}`).length, 0)
})

test('owned cleanup retries failed physical deletion without marking it complete or deleting originals', () => {
  const f = fixture(), uri = 'file:///cache/vibeface-selections/reader', original = 'ph://original'
  f.bytes.set(uri, new Uint8Array([97])); f.bytes.set(original, new Uint8Array([98]))
  const owned = f.privateFiles.ownedFile(new f.File(uri))
  f.failedDeletes.add(uri)
  assert.throws(() => owned.dispose(), /controlled delete failure/)
  assert.equal(f.bytes.has(uri), true)
  assert.throws(() => f.privateFiles.ownedFile(new f.File(original)), /应用内选择副本/)
  f.failedDeletes.clear(); owned.dispose(); owned.dispose()
  assert.deepEqual(f.events, [`file:delete:${uri}`, `file:delete:${uri}`])
  assert.equal(f.bytes.has(uri), false); assert.equal(f.bytes.has(original), true)
})

for (const abandon of ['remove', 'close'] as const) test(`owner observes deferred native cleanup after sibling read failure and ${abandon}`, async () => {
  const f = fixture(), original = 'content://provider/original'
  const uris = ['A', 'B', 'C'].map(name => `file:///cache/vibeface-selections/${name}`)
  for (const uri of [...uris, original]) f.bytes.set(uri, new Uint8Array([97]))
  const files = uris.map(uri => f.privateFiles.ownedFile(new f.File(uri)))
  const errors: AggregateError[] = [], owner = selectionOwner<typeof files[number]>(error => errors.push(error))
  const failure = new Error('first read failed')
  owner.replace(files)
  const operation = owner.use(selected => Promise.all(selected.map(file => file.arrayBuffer())))
  const rejected = assert.rejects(operation, cause => cause === failure)
  if (abandon === 'remove') owner.replace([])
  else owner.close()
  assert.equal(f.events.length, 0)
  f.reads[0].reject(failure)
  await rejected
  assert.equal(f.bytes.has(uris[0]), false)
  assert.equal(f.bytes.has(uris[1]), true)
  assert.equal(f.bytes.has(uris[2]), true)
  owner.replace([]); owner.close(); owner.close()
  assert.equal(errors.length, 0)
  f.failedDeletes.add(uris[1])
  f.reads[1].resolve(); f.reads[2].resolve()
  await tick()
  assert.equal(errors.length, 1)
  assert.match(errors[0].errors[0].message, /controlled delete failure/)
  assert.equal(f.bytes.has(uris[1]), true)
  assert.equal(f.bytes.has(uris[2]), false, 'attempt-all includes a healthy pending sibling')
  assert.equal(f.events.filter(event => event === `file:delete:${uris[1]}`).length, 1)
  f.failedDeletes.clear()
  owner.close(); owner.close()
  assert.equal(f.bytes.has(uris[1]), false)
  assert.equal(errors.length, 1)
  for (const [i, uri] of uris.entries()) assert.equal(f.events.filter(event => event === `file:delete:${uri}`).length, i === 1 ? 2 : 1)
  assert.equal(f.bytes.has(original), true)
  assert.equal(f.events.some(event => event.includes(original)), false)
})

test('native pending dispose reports cleanup independently without replacing a failed reader', async () => {
  const f = fixture(), uri = 'file:///cache/vibeface-selections/A'
  f.bytes.set(uri, new Uint8Array([97]))
  const file = f.privateFiles.ownedFile(new f.File(uri)), failure = new Error('read failure')
  const read = file.text(), rejected = assert.rejects(read, cause => cause === failure)
  const completion = file.dispose()
  assert.ok(completion)
  assert.equal(file.dispose(), completion, 'one completion per cleanup attempt')
  const cleanup = assert.rejects(completion, /controlled delete failure/)
  f.failedDeletes.add(uri); f.reads[0].reject(failure)
  await rejected; await cleanup
  f.failedDeletes.clear(); await file.dispose(); await file.dispose()
  assert.equal(f.bytes.has(uri), false)
  assert.equal(f.events.filter(event => event === `file:delete:${uri}`).length, 2)
})

test('erase settles pending native disposal without reviving the invalidated handle', async () => {
  const f = fixture(), uri = 'file:///cache/vibeface-selections/A', original = 'ph://original'
  f.bytes.set(uri, new Uint8Array([97])); f.bytes.set(original, new Uint8Array([98]))
  f.directories.add('file:///cache/vibeface-selections')
  const file = f.privateFiles.ownedFile(new f.File(uri))
  const read = file.text(), rejected = assert.rejects(read, /正在清除/)
  const completion = file.dispose()
  let cleared = false
  const erase = f.privateFiles.erase(async () => { cleared = true })
  await tick(); assert.equal(cleared, false); assert.equal(f.bytes.has(uri), true)
  f.reads[0].resolve()
  await rejected; await completion; await erase
  assert.equal(cleared, true); assert.equal(f.bytes.has(uri), false); assert.equal(f.bytes.has(original), true)
  await assert.rejects(file.text(), /重新选择/)
  await file.dispose()
  assert.equal(f.events.filter(event => event === `file:delete:${uri}`).length, 1)
})
