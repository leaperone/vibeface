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
