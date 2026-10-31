import { ArrowUpRight } from 'lucide-react'

export function SiteFooter() {
  return (
    <footer className="site-footer">
      <span className="brand">
        <img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />
        Vibeface
      </span>
      <span className="footer-note">自己的设备，共享的 Bot 历史。</span>
      <a className="text-link" href="https://github.com/leaperone/vibeface">
        GitHub <ArrowUpRight size={16} aria-hidden="true" />
      </a>
    </footer>
  )
}
