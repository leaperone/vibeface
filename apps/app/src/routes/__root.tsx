import { claimFocus,Toaster } from '@/components/arc/product-kit'
import { RoutedAction } from '@/components/arc/routed-action'
import { ViewportHeight } from '@/components/viewport-height'
import { installErrorReporting } from '@/lib/error-reporting'
import { registerShell } from '@/lib/offline'
import { webPlatform } from '@/lib/platform'
import { themeScript } from '@/lib/theme'
import { installValidityMessages } from '@/lib/validity'
import { HeadContent,Outlet,Scripts,createRootRoute } from '@tanstack/react-router'
import { DeviceProvider } from '@vibeface/client/identity/context.tsx'
import { useEffect } from 'react'

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
  notFoundComponent: AppNotFound,
})

function AppNotFound() {
  return <main id="main" className="flex h-full min-h-0 flex-col items-center justify-center gap-6 overflow-y-auto p-6 text-center">
    <div className="grid max-w-lg gap-3">
      <h1 ref={claimFocus} tabIndex={-1} className="text-2xl">找不到这个页面</h1>
      <p className="text-secondary">链接可能已失效，或地址输入有误。可以返回消息继续。</p>
    </div>
    <RoutedAction to="/">返回消息</RoutedAction>
  </main>
}

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
