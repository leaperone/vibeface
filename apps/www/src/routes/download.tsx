import { createFileRoute } from '@tanstack/react-router'
import { PageHero } from '@/components/page-hero'
import { SiteLayout } from '@/components/site-layout'

export const Route = createFileRoute('/download')({
  head: () => ({
    meta: [{ title: '下载 Vibeface Hourly' }],
    links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/download' }],
  }),
  component: DownloadPage,
})

function DownloadPage() {
  return (
    <SiteLayout variant="page">
      <PageHero
        eyebrow="Hourly"
        title="从公开 Releases 获取安装包。"
        lede={
          <p>
            当前公开渠道是 Hourly，每小时自动发布。同一个产品 Release 提供电脑版完整包和 Android APK；电脑版包含同版本的 CLI、Core 与网页 App。
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
      <dl className="platform-list" aria-label="支持的平台">
        <div><dt>macOS</dt><dd>Apple Silicon</dd></div>
        <div><dt>Linux</dt><dd>x64</dd></div>
        <div><dt>Linux</dt><dd>ARM64</dd></div>
        <div><dt>Android</dt><dd>APK</dd></div>
      </dl>
      <p className="section-copy">安装器只接受当前版本的完整包。旧开发版数据目录不受支持，首次运行请使用全新目录。需要当前源码时，查看<a href="/docs/quick-start#从源码准备当前版本">源码运行步骤</a>；已有安装请先读<a href="/docs/install">安装与更新说明</a>。要添加浏览器或手机，查看<a href="/docs/onboarding">首次配置与加入设备</a>。</p>
    </SiteLayout>
  )
}
