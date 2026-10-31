import { Link, useRouterState } from '@tanstack/react-router'
import { ThemeToggle } from '@/components/theme-toggle'

const NAV = [
  { label: '文档', kind: 'route' as const, to: '/docs/$' as const, splat: 'overview' },
  { label: '下载', kind: 'href' as const, href: '/download' },
  { label: '社区', kind: 'href' as const, href: '/community' },
  { label: '安全', kind: 'href' as const, href: '/security' },
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
    <header className="site-header">
      <Link className="brand" to="/" aria-label="Vibeface 首页">
        <img className="brand-mark" src="/logo.svg" width="28" height="28" alt="" />
        <span>Vibeface</span>
      </Link>
      <nav aria-label="主导航">
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
        <ThemeToggle />
      </nav>
    </header>
  )
}
