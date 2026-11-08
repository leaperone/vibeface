import { ArrowUpRight } from 'lucide-react'

export function SiteFooter() {
  return (
    <footer className="site-footer" lang="en">
      <div className="site-footer-inner">
        <span className="brand">
          <img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />
          Vibeface
        </span>
        <span className="footer-note">Your computers. One conversation.</span>
        <a className="text-link" href="https://github.com/leaperone/vibeface">
          GitHub <ArrowUpRight size={16} aria-hidden="true" />
        </a>
      </div>
    </footer>
  )
}
