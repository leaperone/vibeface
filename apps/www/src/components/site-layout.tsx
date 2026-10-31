import type { ReactNode } from 'react'
import { SiteFooter } from '@/components/site-footer'
import { SiteHeader } from '@/components/site-header'

type SiteLayoutProps = {
  children: ReactNode
  mainId?: string
  /** Inner marketing pages use a narrower hero column. */
  variant?: 'landing' | 'page'
}

export function SiteLayout({ children, mainId = 'content', variant = 'landing' }: SiteLayoutProps) {
  return (
    <div className={`site site--${variant}`}>
      <a className="site-skip" href={`#${mainId}`}>
        跳到内容
      </a>
      <SiteHeader />
      <main id={mainId} tabIndex={-1}>
        {children}
      </main>
      <SiteFooter />
    </div>
  )
}
