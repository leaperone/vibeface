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
            当前公开渠道是 Nightly。请在 GitHub Releases 核对平台、校验文件和版本，再回到安装文档完成初始化。
          </p>
        }
        actions={
          <>
            <a className="button button-primary" href="https://github.com/leaperone/vibeface/releases">
              打开 GitHub Releases
            </a>
            <a className="button button-quiet" href="/docs/install">
              阅读安装文档
            </a>
          </>
        }
      />
    </SiteLayout>
  )
}
