import { Link, createFileRoute } from '@tanstack/react-router'

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
    <main className="landing">
      <header className="landing-header">
        <Link className="brand" to="/" aria-label="Vibeface 首页">
          <img className="brand-mark" src="/logo.svg" alt="" />
          <span>Vibeface</span>
        </Link>
        <nav aria-label="主导航">
          <Link to="/docs/$" params={{ _splat: 'install' }}>文档</Link>
          <a href="/download">下载</a>
          <a href="/community">社区</a>
          <a href="/security">安全</a>
        </nav>
      </header>

      <section className="landing-hero" aria-labelledby="hero-title">
        <div className="hero-copy-block">
          <p className="hero-kicker"><span className="signal-dot" aria-hidden="true" />个人设备上的 Agent 网络 <span aria-hidden="true">/</span> Nightly</p>
          <h1 id="hero-title">把 Agent 带回你自己的设备。</h1>
          <p className="hero-lede">Vibeface 让 Node、Provider 与个人网络在清晰的授权边界内协作。你的设备运行自己的 Agent，数据留在你的网络。</p>
          <div className="hero-actions">
            <Link className="button button-primary" to="/docs/$" params={{ _splat: 'install' }}>开始安装 <span aria-hidden="true">↗</span></Link>
            <a className="button button-quiet" href="https://github.com/leaperone/vibeface/releases">查看 Nightly Releases <span aria-hidden="true">↗</span></a>
          </div>
          <p className="hero-note">Mac · Linux <span aria-hidden="true">·</span> 不需要网站账号</p>
        </div>

        <div className="hero-visual" role="img" aria-label="Node、Provider 和个人网络围绕 Vibeface 标志连接">
          <div className="visual-grid" aria-hidden="true" />
          <div className="visual-glow" aria-hidden="true" />
          <div className="orbit orbit-one" aria-hidden="true" />
          <div className="orbit orbit-two" aria-hidden="true" />
          <div className="orbit orbit-three" aria-hidden="true" />
          <div className="hero-logo"><img src="/logo.svg" alt="" /></div>
          <div className="visual-node visual-node-top"><span className="node-pip" aria-hidden="true" />Provider<small>your credentials</small></div>
          <div className="visual-node visual-node-left"><span className="node-pip" aria-hidden="true" />Node<small>localhost:4790</small></div>
          <div className="visual-node visual-node-right"><span className="node-pip" aria-hidden="true" />Personal network<small>devices + sync</small></div>
          <span className="visual-caption">01 <span aria-hidden="true">/</span> 03 · AUTHORIZED CONNECTIONS</span>
        </div>
      </section>

      <section className="landing-manifesto" id="system" aria-labelledby="manifesto-title">
        <p className="section-index">01 <span aria-hidden="true">/</span> THE VIBE</p>
        <div>
          <h2 id="manifesto-title">A closer place for your work.</h2>
          <p>Start on your machine. Connect only what you authorize. Keep the boundary visible as your setup grows.</p>
        </div>
        <a className="manifesto-link" href="/security">Understand the boundary <span aria-hidden="true">↗</span></a>
      </section>

      <section className="landing-cards" aria-label="Vibeface 的核心能力">
        <article className="landing-card"><span className="card-index">01</span><h3>Run local.</h3><p>Node starts on your Mac or Linux device and stays close to the work.</p></article>
        <article className="landing-card"><span className="card-index">02</span><h3>Authorize clearly.</h3><p>Provider credentials and device access follow an explicit personal boundary.</p></article>
        <article className="landing-card"><span className="card-index">03</span><h3>Connect deliberately.</h3><p>Bring devices and sync into your network when you choose to.</p></article>
      </section>

      <footer className="landing-footer"><span className="footer-brand"><img className="brand-mark" src="/logo.svg" alt="" /> Vibeface</span><span>Nightly · 2026</span><a href="https://github.com/leaperone/vibeface">GitHub <span aria-hidden="true">↗</span></a></footer>
    </main>
  )
}
