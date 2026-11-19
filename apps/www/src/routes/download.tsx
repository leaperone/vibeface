import { createFileRoute } from '@tanstack/react-router'
import { PageHero } from '@/components/page-hero'
import { SiteLayout } from '@/components/site-layout'

export const Route = createFileRoute('/download')({
  head: () => ({
    meta: [{ title: '下载 Vibeface Hourly' }],
    links: [{ rel: 'canonical', href: 'https://vibeface.dev/download/' }],
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
            Hourly 每小时发布。Desktop、CLI、Android、iOS 和 Web App 使用独立的发布线，下载页只展示当前可安装状态。
          </p>
        }
        actions={
          <>
            <a className="button button-primary" href="https://github.com/leaperone/vibeface/releases" rel="noreferrer">
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
        <div><dt>Android</dt><dd>Hourly APK，手动安装</dd></div>
        <div><dt>iOS</dt><dd>尚未提供可安装包</dd></div>
      </dl>
      <p className="section-copy">Desktop 和 CLI 安装包由 GitHub Releases 提供。Desktop 会在后台检查并准备更新，受管 Node 安装后默认每小时更新。Android 当前仍需从对应的 Mobile Release 下载 APK；iOS 需要等待签名和 TestFlight 分发。网页 App 直接打开<a href="https://app.vibeface.dev/" rel="noreferrer">app.vibeface.dev</a>，重新打开或恢复联网时会检查新版本。已有安装请先读<a href="/docs/install">安装与更新说明</a>。</p>
    </SiteLayout>
  )
}
