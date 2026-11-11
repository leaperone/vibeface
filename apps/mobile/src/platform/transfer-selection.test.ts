import assert from 'node:assert/strict'
import test from 'node:test'
import { transferSelection } from '../../../../packages/client/src/delivery/types.ts'


test('selection replacement and close release private copies after in-flight preparation finishes', async () => {
  const owner = transferSelection()
  const released: string[] = []
  const file = (name: string) => ({ name, size: 1, arrayBuffer: async () => new ArrayBuffer(1), dispose: () => { released.push(name) } })
  owner.replace(file('original selection'))
  let finish = () => {}
  const gate = new Promise<void>(resolve => { finish = resolve })
  const preparing = owner.use(async selected => { await gate; return selected.name })
  owner.replace(file('replacement'))
  assert.deepEqual(released, [])
  owner.close()
  assert.deepEqual(released, ['replacement'])
  finish()
  assert.equal(await preparing, 'original selection')
  assert.deepEqual(released, ['replacement', 'original selection'])
  owner.close()
  owner.replace(file('picker returned after unmount'))
  assert.deepEqual(released, ['replacement', 'original selection', 'picker returned after unmount'])
})

test('failed preparation keeps the selected copy retryable; success releases it once', async () => {
  const owner = transferSelection()
  let releases = 0
  owner.replace({ name: 'selection', size: 1, arrayBuffer: async () => new ArrayBuffer(1), dispose: () => { releases++ } })
  await assert.rejects(owner.use(async () => { throw new Error('offline') }), /offline/)
  assert.equal(releases, 0)
  assert.equal(await owner.use(async selected => selected.name), 'selection')
  owner.replace(null)
  owner.close()
  assert.equal(releases, 1)
  await assert.rejects(owner.use(async () => null), /选择文件/)
})
