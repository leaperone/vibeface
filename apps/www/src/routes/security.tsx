import { createFileRoute } from '@tanstack/react-router'
import { PortalHeader } from '@/components/portal-header'

export const Route = createFileRoute('/security')({
  head: () => ({ meta: [{ title: '安全边界 · Vibeface' }], links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/security' }] }),
  component: SecurityPage,
})

function SecurityPage() {
  return <main className="site-shell"><PortalHeader label="安全" /><section className="hero"><p className="eyebrow">个人网络边界</p><h1>你的设备，你的数据边界。</h1><p className="hero-copy">Node 默认只监听本机。Provider 凭证不进入共享记录，跨设备同步遵循明确授权；部署前请阅读 Node 与排障文档。</p><div className="hero-actions"><a className="button button-primary" href="/docs/node">阅读 Node 文档</a><a className="button button-secondary" href="/docs/troubleshooting">查看排障</a></div></section></main>
}
