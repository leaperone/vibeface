import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { moduleLoader } from '../../../app/src/lib/test-harness.ts'
import { createChatDrafts } from './chat-drafts.ts'

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
