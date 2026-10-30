import { createFileRoute, notFound } from '@tanstack/react-router'
import { DocsBody, DocsDescription, DocsPage, DocsTitle } from 'fumadocs-ui/layouts/docs/page'
import { DocsLayout } from 'fumadocs-ui/layouts/docs'
import { useFumadocsLoader } from 'fumadocs-core/source/client'
import { Suspense, use } from 'react'
import { docs, source } from '@/lib/source'
import { useMDXComponents } from '@/components/mdx'
import { PUBLIC_DOC_BY_SLUG } from '@/lib/public-docs'

type DocsLoaderData = { path: string; url: string; title: string; description: string; pageTree: Awaited<ReturnType<typeof source.serializePageTree>> }

export const Route = createFileRoute('/docs/$')({
  component: DocsRoute,
  head: ({ params }) => {
    const doc = PUBLIC_DOC_BY_SLUG.get(params._splat?.split('/').filter(Boolean)[0] ?? '')
    return doc ? { meta: [{ title: `${doc.title} · Vibeface 文档` }, { name: 'description', content: doc.description }], links: [{ rel: 'canonical', href: `https://www.vibeface.dev/docs/${doc.slug}` }] } : {}
  },
  loader: async ({ params }) => {
    const path = params._splat?.split('/').filter(Boolean) ?? []
    const page = source.getPage(path)
    if (!page) throw notFound()
    await docs.getPage(page.path)?.preload()
    return { path: page.path, url: page.url, title: page.data.title ?? page.path, description: page.data.description ?? 'Vibeface 文档', pageTree: await source.serializePageTree(source.getPageTree()) }
  },
})

function Content({ path }: { path: string }) {
  const page = docs.getPage(path)
  if (!page) throw new Error(`Unknown documentation page: ${path}`)
  const { toc } = use(page.load())
  const MDX = page.body
  return <DocsPage toc={toc}><DocsTitle>{page.title}</DocsTitle><DocsDescription>{page.description}</DocsDescription><DocsBody><MDX components={useMDXComponents()} /></DocsBody></DocsPage>
}

function DocsRoute() {
  const data = Route.useLoaderData() as DocsLoaderData
  const pageTree = useFumadocsLoader({ pageTree: data.pageTree }).pageTree
  return <DocsLayout nav={{ title: 'Vibeface' }} tree={pageTree} searchToggle={{ full: { children: '搜索文档', 'aria-label': '打开文档搜索' }, sm: { children: '搜索', 'aria-label': '打开文档搜索' } }} themeSwitch={{ mode: 'light-dark' }}><Suspense><Content path={data.path} /></Suspense></DocsLayout>
}
