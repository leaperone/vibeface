import { Link, useRouterState } from '@tanstack/react-router'
import { ThemeToggle } from '@/components/theme-toggle'

const NAV = [
  { label: 'Docs', kind: 'route' as const, to: '/docs/$' as const, splat: 'overview' },
  { label: 'Download', kind: 'href' as const, href: '/download' },
  { label: 'Community', kind: 'href' as const, href: '/community' },
  { label: 'Security', kind: 'href' as const, href: '/security' },
]

function navActive(pathname: string, item: (typeof NAV)[number]) {
  if (item.kind === 'href') {
    return pathname === item.href || pathname.startsWith(`${item.href}/`)
  }
  return pathname === '/docs' || pathname.startsWith('/docs/')
}

export function SiteHeader() {
  const pathname = useRouterState({ select: (s) => s.location.pathname })

  return (
    <header className="site-header" lang="en">
      <div className="site-header-inner">
        <Link className="brand" to="/" aria-label="Vibeface home">
          <img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />
          <span>Vibeface</span>
        </Link>
        <nav aria-label="Primary navigation">
          {NAV.map((item) => {
            const active = navActive(pathname, item)
            const className = active ? 'is-active' : undefined
            if (item.kind === 'route') {
              return (
                <Link key={item.label} className={className} to={item.to} params={{ _splat: item.splat }}>
                  {item.label}
                </Link>
              )
            }
            return (
              <a key={item.label} className={className} href={item.href}>
                {item.label}
              </a>
            )
          })}
        </nav>
        <ThemeToggle />
      </div>
    </header>
  )
}
