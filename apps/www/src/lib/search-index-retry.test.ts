import assert from 'node:assert/strict'
import test from 'node:test'
import { flexsearch } from 'fumadocs-core/search/flexsearch'
import { flexsearchStaticClient } from 'fumadocs-core/search/client/flexsearch-static'
import { retrySearchIndex, searchIndexSource } from './search-index-retry.ts'

test('explicit retry escapes the SDK rejected-promise cache and keeps the recovered source across remounts', async (t) => {
  const exported = await flexsearch({ indexes: [{
    id: 'recovery', title: 'Recovery guide', url: '/docs/troubleshooting',
    structuredData: { headings: [{ id: 'recovery', content: 'Recovery' }], contents: [{ heading: 'recovery', content: 'Recovery keeps the original query and result anchor.' }] },
  }] }).export()
  let unavailable = true
  const requests: string[] = []
  t.mock.method(globalThis, 'fetch', async (input: string) => {
    requests.push(input)
    return unavailable ? new Response('Unavailable', { status: 503 }) : Response.json(exported)
  })
  const initialSource = searchIndexSource()
  const initial = flexsearchStaticClient({ from: initialSource })
  await assert.rejects(async () => initial.search('recovery'))
  unavailable = false
  const sameSource = flexsearchStaticClient({ from: initialSource })
  await assert.rejects(async () => sameSource.search('recovery'))
  assert.deepEqual(requests, [initialSource], 'recreating a client cannot escape the rejected promise at the same URL')

  const retrySource = retrySearchIndex()
  assert.notEqual(retrySource, initialSource)
  assert.match(retrySource, /^\/search-index\.json\?retry=\d+$/)
  const recovered = flexsearchStaticClient({ from: retrySource })
  assert.notDeepEqual(recovered.deps, initial.deps, 'the public client dependency change re-runs the unchanged query')
  const results = await recovered.search('recovery')
  assert.ok(results.some(result => result.type === 'page' && result.url === '/docs/troubleshooting'))
  assert.ok(results.some(result => result.url === '/docs/troubleshooting#recovery'))
  assert.deepEqual(requests, [initialSource, retrySource])

  const remountedSource = searchIndexSource()
  assert.equal(remountedSource, retrySource)
  assert.deepEqual(await flexsearchStaticClient({ from: remountedSource }).search('recovery'), results)
  assert.deepEqual(requests, [initialSource, retrySource], 'a remount uses the recovered cache without an automatic request')
  assert.notEqual(retrySearchIndex(), retrySource, 'each further explicit retry gets a fresh cache entry')
})
