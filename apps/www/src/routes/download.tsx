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
            当前公开渠道是 Nightly，支持 Apple Silicon Mac、Linux x64 和 Linux ARM64。完整包包含 CLI、Core 与网页 App；手机 App 使用独立安装渠道。
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
      <p className="section-copy">已有安装？查看<a href="/docs/install">安装与更新说明</a>。要添加浏览器或手机，先读<a href="/docs/onboarding">首次配置与加入设备</a>；可安装版本以实际发布渠道为准。</p>
    </SiteLayout>
  )
}
