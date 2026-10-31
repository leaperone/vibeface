import { Link } from '@tanstack/react-router'
import { PageHero } from '@/components/page-hero'
import { SiteLayout } from '@/components/site-layout'

export function NotFound() {
  return (
    <SiteLayout variant="page">
      <div className="not-found">
        <PageHero
          eyebrow="404 · 页面不存在"
          title="这条路径没有内容。"
          lede={<p>返回首页，或从文档目录重新开始。</p>}
          actions={
            <>
              <Link className="button button-primary" to="/">
                回到首页
              </Link>
              <Link className="button button-quiet" to="/docs/$" params={{ _splat: 'overview' }}>
                打开文档
              </Link>
            </>
          }
        />
      </div>
    </SiteLayout>
  )
}
