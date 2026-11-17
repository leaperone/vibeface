import assert from 'node:assert/strict'
import test from 'node:test'
import type { SearchResult, SearchResultRecord, SearchResultTable } from 'fumadocs-core/search/client'
import { documentSearchPreviews } from './search-previews.ts'

function record(id: string, type: SearchResultRecord['type'], url: string): SearchResultRecord {
  return {
    id, type, url, content: `**恢复** ${id}`, breadcrumbs: ['使用文档', '恢复'],
    hastContent: { type: 'root', children: [{ type: 'element', tagName: 'strong', properties: {}, children: [{ type: 'text', value: `恢复 ${id}` }] }] },
  }
}

test('idle and an actual empty search stay distinct for the native search empty state', () => {
  assert.equal(documentSearchPreviews(undefined), undefined)
  assert.deepEqual(documentSearchPreviews({ query: '不存在', items: [] }), [])
})

test('each article retains its title and highest-ranked heading and passage with exact anchors and highlights', () => {
  const items = [
    record('cli', 'page', '/docs/cli'),
    record('cli-best-text', 'text', '/docs/cli#%E6%81%A2%E5%A4%8D'),
    record('cli-best-heading', 'heading', '/docs/cli#恢复'),
    record('cli-more-text', 'text', '/docs/cli#重试'),
    record('cli-more-heading', 'heading', '/docs/cli#重试'),
    record('history', 'page', '/docs/history-sync'),
    record('history-heading', 'heading', '/docs/history-sync#恢复'),
    record('history-text', 'text', '/docs/history-sync#恢复'),
    record('history-more', 'text', '/docs/history-sync#重复执行'),
    record('cli-related', 'page', '/docs/cli-related'),
    record('related-text', 'text', '/docs/cli-related#恢复'),
  ]
  const result: SearchResult = { query: '恢复', items }
  items.forEach(Object.freeze)
  Object.freeze(items)
  const previews = documentSearchPreviews(result)
  assert.ok(previews)
  assert.deepEqual(previews.map(item => item.id), ['cli', 'cli-best-text', 'cli-best-heading', 'history', 'history-heading', 'history-text', 'cli-related', 'related-text'])
  assert.deepEqual(previews.flatMap(item => item.type === 'page' ? [item.url] : []), ['/docs/cli', '/docs/history-sync', '/docs/cli-related'])
  for (const item of previews) assert.equal(item, items.find(original => original.id === item.id))
  assert.equal(result.query, '恢复')
  assert.equal(result.items.length, 11)
})

test('document identity removes only the fragment, keeping different paths and query URLs independent', () => {
  const items = [
    record('base', 'text', '/docs/cli#恢复'),
    record('same-document', 'text', '/docs/cli#重试'),
    record('query', 'text', '/docs/cli?mode=reference#恢复'),
    record('other', 'text', '/docs/cli-extra#恢复'),
  ]
  assert.deepEqual(documentSearchPreviews({ query: '恢复', items })?.map(item => item.id), ['base', 'query', 'other'])
})

test('table rows share the passage budget while preserving native table headers and selectable row IDs', () => {
  const first = { ...record('cli-row-best', 'text', '/docs/cli#恢复'), table: 'recovery-table' }
  const second = { ...record('cli-row-more', 'text', '/docs/cli#恢复'), table: 'recovery-table' }
  const table: SearchResultTable = {
    type: 'table', id: first.id, columns: 2,
    header: { type: 'root', children: [{ type: 'text', value: '命令 | 用途' }] },
    rows: [first, second],
  }
  Object.freeze(table.rows)
  const result: SearchResult = { query: '恢复', items: [record('cli', 'page', '/docs/cli'), table, record('cli-later', 'text', '/docs/cli#重试'), record('sync', 'page', '/docs/history-sync'), record('sync-text', 'text', '/docs/history-sync#恢复')] }
  const previews = documentSearchPreviews(result)
  assert.ok(previews)
  assert.deepEqual(previews.map(item => item.id), ['cli', first.id, 'sync', 'sync-text'])
  const previewTable = previews[1]
  assert.equal(previewTable.type, 'table')
  if (previewTable.type !== 'table') throw new Error('Expected a native result table')
  assert.equal(previewTable.header, table.header)
  assert.equal(previewTable.columns, 2)
  assert.equal(previewTable.id, previewTable.rows[0].id)
  assert.equal(previewTable.rows[0], first)
  assert.equal(table.rows.length, 2)

  const textFirst: SearchResult = { query: '恢复', items: [record('cli', 'page', '/docs/cli'), record('cli-best', 'text', '/docs/cli#恢复'), table] }
  assert.deepEqual(documentSearchPreviews(textFirst)?.map(item => item.id), ['cli', 'cli-best'])
})
