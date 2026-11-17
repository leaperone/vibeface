import { flexsearchStaticClient } from 'fumadocs-core/search/client/flexsearch-static'
import { useDocsSearch } from 'fumadocs-core/search/client'
import { SearchDialog, SearchDialogClose, SearchDialogContent, SearchDialogFooter, SearchDialogHeader, SearchDialogIcon, SearchDialogInput, SearchDialogList, SearchDialogOverlay } from 'fumadocs-ui/components/dialog/search'
import type { SharedProps } from 'fumadocs-ui/components/dialog/search'
import { useMemo, useRef } from 'react'
import { documentSearchPreviews } from '@/lib/search-previews'

function NoSearchResults() {
  return <div role="status" className="py-12 px-4 text-center text-sm text-fd-muted-foreground">
    <p>没有找到结果。</p>
    <p>试试更短的关键词，或按 Esc 关闭搜索后浏览文档目录。</p>
  </div>
}

export function StaticSearchDialog(props: SharedProps) {
  const opener = useRef<HTMLElement | null>(null)
  const client = useMemo(() => flexsearchStaticClient({ from: '/search-index.json' }), [])
  const search = useDocsSearch({ client })
  const previews = useMemo(() => documentSearchPreviews(search.data), [search.data])
  return <SearchDialog {...props} {...search}>
    <SearchDialogOverlay />
    <SearchDialogContent className="site-search-dialog" onOpenAutoFocus={() => {
      // Radix dispatches this before moving focus into the search field, including Ctrl+K opens.
      opener.current = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null
    }} onCloseAutoFocus={(event) => {
      if (!opener.current) return
      event.preventDefault()
      const target = opener.current.isConnected ? opener.current : document.querySelector<HTMLElement>('main')
      target?.focus({ preventScroll: true })
    }}>
      <SearchDialogHeader><SearchDialogIcon /><SearchDialogInput /><SearchDialogClose /></SearchDialogHeader>
      {search.error ? <div className="site-search-error" role="alert">
        <p>暂时无法搜索文档。可以先使用文档目录，或重新载入后再试。</p>
        <button type="button" onClick={() => window.location.reload()}>重新载入文档</button>
      </div> : <SearchDialogList className="site-search-results" items={previews} Empty={NoSearchResults} />}
      <SearchDialogFooter />
    </SearchDialogContent>
  </SearchDialog>
}
