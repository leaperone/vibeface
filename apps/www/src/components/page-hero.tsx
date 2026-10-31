import type { ReactNode } from 'react'

type PageHeroProps = {
  title: string
  titleId?: string
  eyebrow?: string
  lede?: ReactNode
  actions?: ReactNode
}

export function PageHero({ title, titleId = 'page-title', eyebrow, lede, actions }: PageHeroProps) {
  return (
    <section className="page-hero" aria-labelledby={titleId}>
      {eyebrow ? <p className="hero-kicker">{eyebrow}</p> : null}
      <h1 id={titleId}>{title}</h1>
      {lede ? <div className="page-hero-copy">{lede}</div> : null}
      {actions ? <div className="hero-actions">{actions}</div> : null}
    </section>
  )
}
