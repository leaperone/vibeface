import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight } from 'lucide-react'

export const Route = createFileRoute('/')({
  head: () => ({
    meta: [
      { title: 'Vibeface · 个人设备上的 Agent 网络' },
      { name: 'theme-color', content: '#080b09' },
    ],
    links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/' }],
  }),
  component: HomePage,
})

function HomePage() {
  return (
    <div className="landing">
      <a className="landing-skip" href="#content">跳到内容</a>
      <header className="landing-header">
        <Link className="brand" to="/" aria-label="Vibeface 首页">
          <img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />
          <span>Vibeface</span>
        </Link>
        <nav aria-label="主导航">
          <Link to="/docs/$" params={{ _splat: 'install' }}>文档</Link>
          <a href="/download">下载</a>
          <a href="/community">社区</a>
          <a href="/security">安全</a>
        </nav>
      </header>

      <main id="content" tabIndex={-1}>
        <section className="landing-hero" aria-labelledby="hero-title">
          <div className="hero-copy-block">
            <p className="hero-kicker">个人设备上的 Agent 网络 <span aria-hidden="true">/</span> Nightly</p>
            <h1 id="hero-title">把 Agent 带回你自己的设备。</h1>
            <p className="hero-lede">在 Mac 或 Linux 上运行 Agent，连接你授权的设备与 Provider。你的设备，组成你的个人网络。</p>
            <div className="hero-actions">
              <Link className="button button-primary" to="/docs/$" params={{ _splat: 'install' }}>开始安装 <ArrowRight size={16} aria-hidden="true" /></Link>
              <a className="button button-quiet" href="https://github.com/leaperone/vibeface/releases">Nightly Releases <ArrowUpRight size={16} aria-hidden="true" /></a>
            </div>
          </div>

          <figure className="hero-figure" aria-label="Vibeface 连接你的 Node、Provider 与个人网络">
            <div className="hero-visual">
              <div className="visual-grid" aria-hidden="true" />
              <div className="visual-glow" aria-hidden="true" />
              <div className="orbit orbit-one" aria-hidden="true" />
              <div className="orbit orbit-two" aria-hidden="true" />
              <div className="orbit orbit-three" aria-hidden="true" />
              <div className="hero-logo"><img src="/logo.svg" width="144" height="144" alt="" /></div>
              <a className="visual-node visual-node-top" href="/docs/deployment#配置-sdk-与-app-server"><span>Provider</span><small>你的模型与凭证</small></a>
              <a className="visual-node visual-node-left" href="/docs/node"><span>Node</span><small>运行在你的设备</small></a>
              <a className="visual-node visual-node-right" href="/docs/node#node-之间的连接"><span>个人网络</span><small>连接设备与同步</small></a>
            </div>
            <figcaption>由你的设备组成，按你的授权连接。</figcaption>
          </figure>
        </section>

        <section className="landing-system" id="system" aria-labelledby="system-title">
          <div className="system-intro">
            <h2 id="system-title">设备属于你。<br />连接由你决定。</h2>
            <p>从一台电脑开始，按需接入 Provider，再连接其他设备。Vibeface 让它们在明确的授权边界内协作。</p>
            <a className="text-link" href="/security">了解安全边界 <ArrowRight size={16} aria-hidden="true" /></a>
          </div>
          <div className="system-details">
            <article><h3>在本机运行</h3><p>在 Mac 或 Linux 上启动 Node，从 localhost 打开工作区，选择 Agent 可以访问的项目目录。</p></article>
            <article><h3>按授权接入</h3><p>使用你已有的 Provider。模型凭证保留在本机，不进入个人网络的共享记录。</p></article>
            <article><h3>连接自己的设备</h3><p>通过邀请加入同一个个人网络，同步会话与历史，让其他设备接着工作。</p></article>
          </div>
        </section>
      </main>

      <footer className="landing-footer">
        <span className="brand"><img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />Vibeface</span>
        <span className="footer-note">个人设备上的 Agent 网络</span>
        <a className="text-link" href="https://github.com/leaperone/vibeface">GitHub <ArrowUpRight size={16} aria-hidden="true" /></a>
      </footer>
    </div>
  )
}
