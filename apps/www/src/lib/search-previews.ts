import type { SearchResult, SearchResultItem, SearchResultRecord } from 'fumadocs-core/search/client'

/** Results arrive in relevance order within each document; preview its best heading and passage. */
export function documentSearchPreviews(result: SearchResult | undefined): SearchResultItem[] | undefined {
  if (!result) return undefined
  const previews = new Map<string, Set<SearchResultRecord['type']>>()
  function retain(record: SearchResultRecord) {
    if (record.type === 'page') return true
    const document = record.url.split('#')[0]
    let types = previews.get(document)
    if (!types) previews.set(document, types = new Set())
    if (types.has(record.type)) return false
    types.add(record.type)
    return true
  }

  return result.items.flatMap((item): SearchResultItem[] => {
    if (item.type !== 'table') return retain(item) ? [item] : []
    const rows = item.rows.filter(retain)
    if (!rows.length) return []
    return [rows.length === item.rows.length ? item : { ...item, id: rows[0].id, rows }]
  })
}
