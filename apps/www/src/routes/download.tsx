import { createFileRoute } from '@tanstack/react-router'
import { PortalHeader } from '@/components/portal-header'

export const Route = createFileRoute('/download')({
  head: () => ({ meta: [{ title: '下载 Vibeface Nightly' }], links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/download' }] }),
  component: DownloadPage,
})

function DownloadPage() {
  return <main className="site-shell"><PortalHeader label="下载" /><section className="hero"><p className="eyebrow">Nightly</p><h1>从公开 Releases 获取安装包。</h1><p className="hero-copy">当前公开渠道是 Nightly。请在 GitHub Releases 核对平台、校验文件和版本，再回到安装文档完成初始化。</p><div className="hero-actions"><a className="button button-primary" href="https://github.com/leaperone/vibeface/releases">打开 GitHub Releases</a><a className="button button-secondary" href="/docs/install">阅读安装文档</a></div></section></main>
}
