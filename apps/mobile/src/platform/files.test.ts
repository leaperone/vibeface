import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
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
  return { privateFiles, File, bytes, copies, events, failedDeletes }
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
