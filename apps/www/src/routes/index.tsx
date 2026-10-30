import { Link, createFileRoute } from '@tanstack/react-router'
import { ThemeToggle } from '@/components/theme-toggle'

export const Route = createFileRoute('/')({
  head: () => ({ meta: [{ title: 'Vibeface · 个人设备上的 Agent 网络' }], links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/' }] }),
  component: HomePage,
})

function HomePage() {
  return (
    <main className="site-shell">
      <header className="site-header">
        <Link className="brand" to="/">Vibeface</Link>
        <nav aria-label="主导航">
          <Link to="/docs/$" params={{ _splat: 'install' }}>文档</Link>
          <a href="/download">下载</a>
          <a href="/community">社区</a>
          <a href="/security">安全</a>
          <ThemeToggle />
        </nav>
      </header>
      <section className="hero" aria-labelledby="hero-title">
        <p className="eyebrow">个人设备上的 Agent 网络 · Nightly</p>
        <h1 id="hero-title">把 Agent 带回你自己的设备。</h1>
        <p className="hero-copy">Vibeface 在你的 Mac 或 Linux 上运行 Node 与 Agent。Provider 保持原有边界，数据留在你的个人网络。</p>
        <div className="hero-actions">
          <Link className="button button-primary" to="/docs/$" params={{ _splat: 'install' }}>开始安装</Link>
          <a className="button button-secondary" href="https://github.com/leaperone/vibeface/releases">查看 Nightly Releases</a>
        </div>
      </section>
      <section className="system-map" aria-label="Vibeface 功能边界">
        <div className="system-map-copy"><p className="eyebrow">真实工作流</p><h2>从本机开始，沿授权边界连接。</h2><p>安装后打开 localhost，Node 只负责自己的设备、个人网络和明确配置的 Provider。</p></div>
        <div className="system-diagram" aria-label="本机 Node、Provider 与个人网络关系图"><div className="diagram-node diagram-node-main"><span>Node</span><small>localhost:4790</small></div><div className="diagram-line diagram-line-top" /><div className="diagram-node diagram-node-provider"><span>Provider</span><small>你的凭证</small></div><div className="diagram-line diagram-line-bottom" /><div className="diagram-node diagram-node-network"><span>个人网络</span><small>设备与同步</small></div></div>
      </section>
      <footer className="site-footer"><span>Vibeface Nightly</span><a href="https://github.com/leaperone/vibeface">GitHub</a></footer>
    </main>
  )
}
