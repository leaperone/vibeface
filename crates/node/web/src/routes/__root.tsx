import { HeadContent, Scripts, createRootRoute } from '@tanstack/react-router'
import { ViewportHeight } from '@/components/viewport-height'

import appCss from '../styles.css?url'

export const Route = createRootRoute({
  head: () => ({
    meta: [
      {
        charSet: 'utf-8',
      },
      {
        name: 'viewport',
        content: 'width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content',
      },
      {
        title: 'Vibeface',
      },
    ],
    links: [
      {
        rel: 'stylesheet',
        href: appCss,
      },
    ],
  }),
  shellComponent: RootDocument,
})

function RootDocument({ children }: { children: React.ReactNode }) {
  return (
    <html lang="zh-CN">
      <head>
        <HeadContent />
      </head>
      <body>
        <ViewportHeight />
        <p data-testid="vibeface-shell" hidden>网页壳已加载。</p>
        {children}
        <Scripts />
      </body>
    </html>
  )
}
