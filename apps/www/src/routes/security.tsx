import { createFileRoute } from '@tanstack/react-router'
import { PortalHeader } from '@/components/portal-header'

export const Route = createFileRoute('/security')({
  head: () => ({ meta: [{ title: '安全边界 · Vibeface' }, { name: 'description', content: '了解设备身份、加密连接与本机执行的信任边界。' }], links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/security' }] }),
  component: SecurityPage,
})

function SecurityPage() {
  return (
    <main className="site-shell">
      <PortalHeader label="安全" />
      <section className="hero">
        <h1>先了解信任边界。</h1>
        <p className="hero-copy">设备各用独立密钥，加入需要批准，连接验证身份并加密。配置的 Provider 凭证留在本机，授权 Node 与所选模型服务仍会处理任务内容。</p>
        <p className="hero-copy">传输加密不等于磁盘静态加密，项目目录检查不是操作系统沙箱。网页 HTTP 入口默认回环监听，Node 间 iroh UDP 默认绑定全接口。</p>
        <div className="hero-actions">
          <a className="button button-primary" href="/docs/privacy-security">阅读隐私与权限说明</a>
          <a className="button button-secondary" href="/docs/deployment#配置网络入口">配置网络入口</a>
        </div>
      </section>
    </main>
  )
}
