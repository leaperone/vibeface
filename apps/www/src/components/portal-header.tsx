import { ThemeToggle } from '@/components/theme-toggle'

export function PortalHeader({ label }: { label?: string }) {
  return (
    <header className="site-header">
      <a className="brand" href="/" aria-label="Vibeface 首页">
        <img className="brand-mark" src="/logo.svg" alt="" />
        <span>Vibeface</span>
      </a>
      <nav aria-label="主导航">
        <a href="/docs/install">文档</a>
        <a href="/download">下载</a>
        <a href="/community">社区</a>
        <a href="/security">安全</a>
        <ThemeToggle />
      </nav>
      {label ? <span className="eyebrow page-label">{label}</span> : null}
    </header>
  )
}
