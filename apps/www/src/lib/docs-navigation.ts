import type * as PageTree from 'fumadocs-core/page-tree'
import { PUBLIC_DOC_GROUPS } from './public-docs.ts'

/** Keep source page nodes intact so titles, URLs and active-page lookup stay canonical. */
export function groupDocsNavigation(tree: PageTree.Root): PageTree.Root {
  const pages = new Map<string, PageTree.Item>()
  for (const node of tree.children) {
    if (node.type !== 'page') throw new Error('Expected a flat public documentation source tree')
    if (pages.has(node.url)) throw new Error(`Duplicate documentation page: ${node.url}`)
    pages.set(node.url, node)
  }

  const children: PageTree.Folder[] = PUBLIC_DOC_GROUPS.map(({ title, pages: slugs }) => ({
    type: 'folder',
    name: title,
    collapsible: true,
    defaultOpen: false,
    children: slugs.map(slug => {
      const url = `/docs/${slug}`
      const page = pages.get(url)
      if (!page) throw new Error(`Missing documentation page: ${url}`)
      pages.delete(url)
      return page
    }),
  }))
  if (pages.size) throw new Error(`Unregistered documentation pages: ${[...pages.keys()].join(', ')}`)
  return { ...tree, children }
}
