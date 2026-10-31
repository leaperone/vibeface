import { createFileRoute } from '@tanstack/react-router'
import { PageHero } from '@/components/page-hero'
import { SiteLayout } from '@/components/site-layout'

export const Route = createFileRoute('/community')({
  head: () => ({
    meta: [{ title: '社区 · Vibeface' }],
    links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/community' }],
  }),
  component: CommunityPage,
})

function CommunityPage() {
  return (
    <SiteLayout variant="page">
      <PageHero
        eyebrow="公开协作"
        title="从仓库开始交流。"
        lede={
          <p>
            Vibeface 的 GitHub 仓库是公开的。Discussions 尚未启用，问题、改进和文档建议请先通过 Issues
            了解项目状态。
          </p>
        }
        actions={
          <a className="button button-primary" href="https://github.com/leaperone/vibeface">
            打开 GitHub 仓库
          </a>
        }
      />
    </SiteLayout>
  )
}
