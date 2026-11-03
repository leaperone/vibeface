import { HeadContent, Outlet, Scripts, createRootRoute } from '@tanstack/react-router'
import { useEffect } from 'react'
import { Toaster } from '@/components/app/kit'
import { DeviceProvider } from '@vibeface/client/identity/context.tsx'
import { ViewportHeight } from '@/components/viewport-height'
import { registerShell } from '@/lib/offline'
import { installErrorReporting } from '@/lib/error-reporting'
import { webPlatform } from '@/lib/platform'
import { installValidityMessages } from '@/lib/validity'
import { themeScript } from '@/lib/theme'

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
    scripts: [{ children: themeScript }],
    links: [
      {
        rel: 'stylesheet',
        href: appCss,
      },
      { rel: 'manifest', href: '/manifest.webmanifest' },
      { rel: 'icon', href: '/icon.svg', type: 'image/svg+xml' },
    ],
  }),
  shellComponent: RootDocument,
  component: RootLayout,
})

/** DeviceProvider reads and clears invitation hashes on mount, so it wraps every route including /join. */
function RootLayout() {
  return <DeviceProvider platform={webPlatform}><Outlet /><Toaster /></DeviceProvider>
}

function RootDocument({ children }: { children: React.ReactNode }) {
  useEffect(registerShell, [])
  useEffect(installErrorReporting, [])
  useEffect(installValidityMessages, [])
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
