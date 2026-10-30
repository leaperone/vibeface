import { flexsearchStaticClient } from 'fumadocs-core/search/client/flexsearch-static'
import { useDocsSearch } from 'fumadocs-core/search/client'
import { SearchDialog, SearchDialogClose, SearchDialogContent, SearchDialogFooter, SearchDialogHeader, SearchDialogIcon, SearchDialogInput, SearchDialogList, SearchDialogOverlay } from 'fumadocs-ui/components/dialog/search'
import type { SharedProps } from 'fumadocs-ui/components/dialog/search'
import { useMemo } from 'react'

export function StaticSearchDialog(props: SharedProps) {
  const client = useMemo(() => flexsearchStaticClient({ from: '/search-index.json' }), [])
  const search = useDocsSearch({ client })
  return <SearchDialog {...props} {...search}>
    <SearchDialogOverlay />
    <SearchDialogContent>
      <SearchDialogHeader><SearchDialogIcon /><SearchDialogInput /><SearchDialogClose /></SearchDialogHeader>
      <SearchDialogList />
      <SearchDialogFooter />
    </SearchDialogContent>
  </SearchDialog>
}
