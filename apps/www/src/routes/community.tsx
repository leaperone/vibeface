import { createFileRoute } from '@tanstack/react-router'
import { PortalHeader } from '@/components/portal-header'

export const Route = createFileRoute('/community')({
  head: () => ({ meta: [{ title: '社区 · Vibeface' }], links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/community' }] }),
  component: CommunityPage,
})

function CommunityPage() {
  return <main className="site-shell"><PortalHeader label="社区" /><section className="hero"><p className="eyebrow">公开协作</p><h1>从仓库开始交流。</h1><p className="hero-copy">Vibeface 的 GitHub 仓库是公开的。Discussions 尚未启用，问题、改进和文档建议请先通过 Issues 了解项目状态。</p><div className="hero-actions"><a className="button button-primary" href="https://github.com/leaperone/vibeface">打开 GitHub 仓库</a></div></section></main>
}
