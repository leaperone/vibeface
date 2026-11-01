import { createFileRoute, notFound } from '@tanstack/react-router'
import { DocsBody, DocsDescription, DocsPage, DocsTitle } from 'fumadocs-ui/layouts/docs/page'
import { DocsLayout } from 'fumadocs-ui/layouts/docs'
import { useFumadocsLoader } from 'fumadocs-core/source/client'
import { Suspense, use } from 'react'
import { Link } from '@tanstack/react-router'
import { docs, source } from '@/lib/source'
import { useMDXComponents } from '@/components/mdx'

export const Route = createFileRoute('/docs/$')({
  component: DocsRoute,
  loader: async ({ params }) => {
    const path = params._splat?.split('/').filter(Boolean) ?? []
    const page = source.getPage(path.length ? path : ['overview'])
    if (!page) throw notFound()
    await docs.getPage(page.path)?.preload()
    return {
      path: page.path,
      url: page.url,
      title: page.data.title ?? page.path,
      description: page.data.description ?? 'Vibeface 文档',
      pageTree: await source.serializePageTree(source.getPageTree()),
    }
  },
  head: ({ loaderData }) => loaderData
    ? {
        meta: [
          { title: `${loaderData.title} · Vibeface 文档` },
          { name: 'description', content: loaderData.description },
        ],
        links: [{ rel: 'canonical', href: `https://www.vibeface.dev${loaderData.url}` }],
      }
    : {},
})

function Content({ path }: { path: string }) {
  const page = docs.getPage(path)
  if (!page) throw new Error(`Unknown documentation page: ${path}`)
  const { toc } = use(page.load())
  const MDX = page.body
  return (
    <DocsPage toc={toc}>
      <DocsTitle>{page.title}</DocsTitle>
      <DocsDescription>{page.description}</DocsDescription>
      <DocsBody>
        <MDX components={useMDXComponents()} />
      </DocsBody>
    </DocsPage>
  )
}

const docsNavLinks = [
  { text: '下载', url: '/download' },
  { text: '社区', url: '/community' },
  { text: '安全', url: '/security' },
]

function DocsRoute() {
  const data = Route.useLoaderData()
  const pageTree = useFumadocsLoader({ pageTree: data.pageTree }).pageTree
  return (
    <div className="site-docs">
      <DocsLayout
        nav={{
          title: (
            <Link className="brand docs-brand" to="/">
              <img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />
              <span>Vibeface</span>
            </Link>
          ),
          url: '/',
        }}
        links={docsNavLinks}
        tree={pageTree}
        searchToggle={{
          full: { children: '搜索文档', 'aria-label': '打开文档搜索' },
          sm: { children: '搜索', 'aria-label': '打开文档搜索' },
        }}
        themeSwitch={{ mode: 'light-dark' }}
      >
        <Suspense>
          <Content path={data.path} />
        </Suspense>
      </DocsLayout>
    </div>
  )
}
