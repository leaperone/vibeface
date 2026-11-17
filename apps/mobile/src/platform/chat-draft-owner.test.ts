import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { hookHarness, moduleLoader } from '../../../app/src/lib/test-harness.ts'
import { createChatDrafts } from './chat-drafts.ts'
import { selectionOwner } from '../../../../packages/client/src/lib/selection.ts'
import type { DraftScope } from './chat-drafts.ts'
import type { chatDrafts } from './native-chat-drafts.ts'
import type { privateFiles } from './files.ts'
import type { useChatDraft } from '../features/chat/use-chat-draft.ts'

async function fixture() {
  let revision = 0
  const values = new Map<string, string>(), files = new Set(['private:original'])
  const scope = { networkId: 'network', deviceId: 'phone', conversationId: 'chat', inputTarget: 'main' }
  const repository = createChatDrafts({
    storage: { get: (key) => values.get(key), set: (key, value) => { values.set(key, value) }, remove: (key) => { values.delete(key) } },
    revision: () => `revision-${++revision}`,
    files: {
      owns: (uri) => uri.startsWith('private:'), exists: (uri) => files.has(uri), release: (uri) => { files.delete(uri) },
      retain: async (file) => { const retained = { ...file, uri: `private:copy-${++revision}` }; files.add(retained.uri); return retained },
    },
  })
  await repository.write(scope, { text: 'submitted text', files: [{ uri: 'private:original', name: 'report.txt', size: 3, mimeType: null }] })
  const load = moduleLoader(createContext({}), (specifier) => {
    if (specifier === 'react') return {
      useMemo: (factory: () => unknown) => factory(), useRef: (value: unknown) => ({ current: value }),
      useState: (initial: unknown) => [typeof initial === 'function' ? initial() : initial, () => {}], useEffect() {},
    }
    if (specifier === '@/platform/native-chat-drafts') return { chatDrafts: repository }
    if (specifier === '@vibeface/client/lib/selection.ts') return { selectionOwner }
    if (specifier === '@/platform/files') return { privateFiles: { run: async (operation: (valid: () => boolean) => Promise<unknown>) => operation(() => true) } }
    throw new Error(`Unexpected dependency: ${specifier}`)
  })
  const hook = load(fileURLToPath(new URL('../features/chat/use-chat-draft.ts', import.meta.url))).useChatDraft
  assert(typeof hook === 'function')
  const owner: unknown = Reflect.apply(hook, undefined, [scope])
  assert(owner && typeof owner === 'object' && 'flush' in owner && typeof owner.flush === 'function' && 'clear' in owner && typeof owner.clear === 'function' && 'changeText' in owner && typeof owner.changeText === 'function')
  const flushMethod = owner.flush, clearMethod = owner.clear, changeMethod = owner.changeText
  const flush = () => Promise.resolve(Reflect.apply(flushMethod, owner, []))
  const clear = (snapshot: unknown) => Promise.resolve(Reflect.apply(clearMethod, owner, [snapshot]))
  const changeText = (text: string) => Reflect.apply(changeMethod, owner, [text])
  return { repository, scope, files, flush, clear, changeText }
}

test('an old send cannot clear a newer edit queued before its attachment write starts', async () => {
  const f = await fixture(), submitted = await f.flush()
  const lease = f.repository.lease(f.repository.read(f.scope))
  f.changeText('new text typed while the attachment was preparing')
  const clearing = f.clear(submitted)
  lease.release()
  assert.equal(await clearing, false)
  await f.flush()
  const recovered = f.repository.read(f.scope)
  assert.equal(recovered.text, 'new text typed while the attachment was preparing')
  assert.equal(recovered.files.length, 1)
  assert.ok(f.files.has(recovered.files[0].uri))
})

function deferred() {
  let resolve = () => {}
  const promise = new Promise<void>(done => { resolve = done })
  return { promise, resolve }
}

/** Run all ownership layers; replace only native filesystem, MMKV and React scheduling. */
function nativeFixture() {
  const contents = new Map<string, Uint8Array>(), values = new Map<string, string>()
  const copies = deferred(), copying = deferred(), reading = deferred(), finishRead = deferred()
  let delayRead = false
  let sequence = 0
  class Directory {
    uri: string
    constructor(parent: { uri: string }, name: string) { this.uri = `${parent.uri}/${name}` }
    get exists() { return [...contents.keys()].some(uri => uri.startsWith(`${this.uri}/`)) }
    create() {}
    list() { return [...contents.keys()].filter(uri => uri.startsWith(`${this.uri}/`)).map(uri => new File(uri)) }
    delete() { for (const file of this.list()) file.delete() }
  }
  class File {
    uri: string
    constructor(parent: string | { uri: string }, name?: string) { this.uri = typeof parent === 'string' ? parent : `${parent.uri}/${name}` }
    get exists() { return contents.has(this.uri) }
    get size() { return contents.get(this.uri)?.byteLength ?? 0 }
    async copy(destination: File) {
      copying.resolve()
      await copies.promise
      const bytes = contents.get(this.uri)
      assert(bytes, 'the staging selection must survive until its durable copy completes')
      contents.set(destination.uri, bytes.slice())
    }
    async arrayBuffer() {
      if (delayRead) { reading.resolve(); await finishRead.promise }
      const bytes = contents.get(this.uri)
      assert(bytes, 'read must use a live native file')
      return bytes.slice().buffer
    }
    async text() { return 'report' }
    delete() { contents.delete(this.uri) }
  }
  const hooks = hookHarness(true)
  const load = moduleLoader(createContext({ crypto: { randomUUID: () => `native-${++sequence}` }, AggregateError }), (specifier): unknown => {
    if (specifier === 'react') return hooks.react
    if (specifier === 'expo-file-system') return { File, Directory, Paths: { cache: { uri: 'cache:' }, document: { uri: 'document:', list: () => [] } } }
    if (specifier === 'react-native-mmkv') return { createMMKV: () => ({
      getString: (key: string) => values.get(key), set: (key: string, value: string) => { values.set(key, value) },
      remove: (key: string) => { values.delete(key) }, getAllKeys: () => [...values.keys()], clearAll: () => values.clear(),
    }) }
    if (specifier === '@vibeface/client/lib/node.ts') return { record() { throw new Error('No root recovery files exist in this fixture') } }
    if (specifier === '@vibeface/client/lib/selection.ts') return { selectionOwner }
    if (specifier === './chat-drafts') return { createChatDrafts }
    if (specifier === './files' || specifier === '@/platform/files') return load(fileURLToPath(new URL('./files.ts', import.meta.url)))
    if (specifier === '@/platform/native-chat-drafts') return load(fileURLToPath(new URL('./native-chat-drafts.ts', import.meta.url)))
    throw new Error(`Unexpected dependency: ${specifier}`)
  })
  const native = load(fileURLToPath(new URL('./native-chat-drafts.ts', import.meta.url))).chatDrafts
  assert(native && typeof native === 'object')
  const repository = {
    read(...args: Parameters<typeof chatDrafts.read>): ReturnType<typeof chatDrafts.read> { return Reflect.apply(Reflect.get(native, 'read'), native, args) },
    write(...args: Parameters<typeof chatDrafts.write>): ReturnType<typeof chatDrafts.write> { return Reflect.apply(Reflect.get(native, 'write'), native, args) },
    lease(...args: Parameters<typeof chatDrafts.lease>): ReturnType<typeof chatDrafts.lease> { return Reflect.apply(Reflect.get(native, 'lease'), native, args) },
  }
  const files = load(fileURLToPath(new URL('./files.ts', import.meta.url))).privateFiles
  assert(files && typeof files === 'object')
  const hook = load(fileURLToPath(new URL('../features/chat/use-chat-draft.ts', import.meta.url))).useChatDraft
  assert(typeof hook === 'function')
  const scope = { networkId: 'network', deviceId: 'phone', conversationId: 'chat', inputTarget: 'main' }
  const render = (nextScope: DraftScope = scope): ReturnType<typeof useChatDraft> => {
    hooks.reset()
    const result = Reflect.apply(hook, undefined, [nextScope])
    for (const effect of hooks.afterRender.splice(0)) effect()
    return result
  }
  const uri = 'cache:/vibeface-selections/report'
  contents.set(uri, new Uint8Array([1, 2, 3]))
  const pick: ReturnType<typeof privateFiles.ownedFile> = Reflect.apply(Reflect.get(files, 'ownedFile'), files, [new File(uri)])
  return {
    scope, render, repository, contents, copies, copying, reading, finishRead,
    delayRead() { delayRead = true },
    picked: { ...pick, name: 'report.txt', mimeType: 'text/plain' },
    erase: (): ReturnType<typeof privateFiles.erase> => Reflect.apply(Reflect.get(files, 'erase'), files, [async () => {}]),
  }
}

test('adding a pick holds its staging file through a pending copy and an independent scope change', async () => {
  const f = nativeFixture(), original = f.render()
  const adding = original.addFiles([f.picked])
  await f.copying.promise
  assert.equal(f.contents.has(f.picked.uri), true)
  const otherScope = { ...f.scope, conversationId: 'other-chat' }
  const other = f.render(otherScope)
  other.changeText('different conversation')
  await other.flush()
  assert.equal(f.contents.has(f.picked.uri), true)
  f.copies.resolve()
  await adding
  assert.equal(f.contents.has(f.picked.uri), false, 'the staging owner closes after persistence')
  await assert.rejects(f.picked.arrayBuffer(), /重新选择/)
  const saved = f.repository.read(f.scope)
  assert.equal(saved.files.length, 1)
  assert.notEqual(saved.files[0].uri, f.picked.uri)
  assert.deepEqual(Object.keys(saved.files[0]).sort(), ['mimeType', 'name', 'size', 'uri'])
  const lease = f.repository.lease(saved)
  assert.deepEqual([...new Uint8Array(await lease.files[0].arrayBuffer())], [1, 2, 3])
  lease.release()
  assert.equal(f.render(otherScope).text, 'different conversation')
  assert.equal(f.repository.read(otherScope).files.length, 0)
  const restored = f.render()
  const submitted = await restored.flush()
  assert.equal(await restored.clear(submitted), true)
  assert.equal(f.contents.has(saved.files[0].uri), false)
})

test('erasing during attachment persistence waits for the copy and closes its staging owner on failure', async () => {
  const f = nativeFixture(), draft = f.render()
  const adding = draft.addFiles([f.picked])
  const rejected = assert.rejects(adding, /正在清除/)
  await f.copying.promise
  let erased = false
  const erasing = f.erase().then(() => { erased = true })
  await Promise.resolve()
  assert.equal(erased, false)
  assert.equal(f.contents.has(f.picked.uri), true)
  f.copies.resolve()
  await rejected
  await erasing
  assert.equal(f.contents.size, 0)
  assert.equal(f.repository.read(f.scope).files.length, 0)
  await assert.rejects(f.picked.arrayBuffer(), /重新选择/)
})

test('successful native persistence retires an owned pick without deleting bytes under an active picked reader', async () => {
  const f = nativeFixture(), draft = f.render()
  f.delayRead()
  const bytes = f.picked.arrayBuffer()
  await f.reading.promise
  const adding = draft.addFiles([f.picked])
  await f.copying.promise
  const otherScope = { ...f.scope, conversationId: 'metadata-only-writer' }
  const otherWrite = f.repository.write(otherScope, { text: '', files: [{ uri: f.picked.uri, name: f.picked.name, size: f.picked.size, mimeType: f.picked.mimeType }] })
  f.copies.resolve()
  await adding
  await otherWrite
  assert.equal(f.contents.has(f.picked.uri), true, 'owned disposal waits for active picked reads')
  await assert.rejects(f.picked.arrayBuffer(), /重新选择/)
  f.finishRead.resolve()
  assert.deepEqual([...new Uint8Array(await bytes)], [1, 2, 3])
  assert.equal(f.contents.has(f.picked.uri), false, 'the last picked reader completes physical disposal')
  const saved = f.repository.read(f.scope)
  assert.equal(f.contents.has(saved.files[0].uri), true)
  assert.notEqual(f.repository.read(otherScope).files[0].uri, saved.files[0].uri)
  const lease = f.repository.lease(saved)
  assert.deepEqual([...new Uint8Array(await lease.files[0].arrayBuffer())], [1, 2, 3])
  lease.release()
})

test('a successful unchanged send clears the submitted revision and subsequent typing starts a new draft', async () => {
  const f = await fixture(), submitted = await f.flush()
  const lease = f.repository.lease(f.repository.read(f.scope))
  const clearing = f.clear(submitted)
  lease.release()
  assert.equal(await clearing, true)
  assert.equal(f.repository.read(f.scope).text, '')
  f.changeText('next message')
  await f.flush()
  const next = f.repository.read(f.scope)
  assert.equal(next.text, 'next message')
  assert.equal(next.files.length, 0)
})
