import { createFileRoute } from '@tanstack/react-router'
import { PageHero } from '@/components/page-hero'
import { SiteLayout } from '@/components/site-layout'

export const Route = createFileRoute('/download')({
  head: () => ({
    meta: [{ title: '下载 Vibeface Nightly' }],
    links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/download' }],
  }),
  component: DownloadPage,
})

function DownloadPage() {
  return (
    <SiteLayout variant="page">
      <PageHero
        eyebrow="Nightly"
        title="从公开 Releases 获取安装包。"
        lede={
          <p>
            当前公开渠道是 Nightly，支持 Apple Silicon Mac、Linux x64 和 Linux ARM64。请核对包内是否包含当前版本要求的 CLI、Core 与网页 App；手机 App 使用独立安装渠道。
          </p>
        }
        actions={
          <>
            <a className="button button-primary" href="https://github.com/leaperone/vibeface/releases">
              打开 GitHub Releases
            </a>
            <a className="button button-quiet" href="/docs/quick-start">
              跟着快速上手安装
            </a>
          </>
        }
      />
      <p className="section-copy">Nightly 0.0.1-nightly.20261031.18 仍是拆分前的包，不能用当前 main 的安装器安装。需要当前源码时，查看<a href="/docs/quick-start#从源码准备当前版本">源码运行步骤</a>；已有安装请先读<a href="/docs/install">安装与更新说明</a>。要添加浏览器或手机，查看<a href="/docs/onboarding">首次配置与加入设备</a>。</p>
    </SiteLayout>
  )
}
