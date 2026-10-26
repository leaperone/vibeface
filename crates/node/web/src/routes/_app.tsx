import { lazy, Suspense, useState } from 'react'
import { Navigate, Outlet, createFileRoute, useNavigate, useRouterState } from '@tanstack/react-router'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useOnboarding } from '@/features/identity/onboarding-flag'
import { WorkspaceProvider } from '@vibeface/client/workspace/context.tsx'
import { ShellSkeleton, WorkspaceShell } from '@/features/workspace/workspace-shell'
import { webPlatform } from '@/lib/platform'

const Onboarding = lazy(() => import('@/features/identity/onboarding').then((module) => ({ default: module.Onboarding })))

export const Route = createFileRoute('/_app')({ component: AppLayout })

/** Workspace pages need a member connection; a browser with saved credentials waits here while it reconnects. */
function AppLayout() {
  const { ready, phase, entries, initialInvitation, tabs, joining, link, disconnect, retryNow } = useDevice()
  const navigate = useNavigate()
  const onboarding = useOnboarding()
  // Onboarding ends on a pressed button that leaves with it; the workspace that follows takes the focus on its title.
  const [onboarded] = useState(onboarding !== null)
  // The page that asked, not the connect page this layout is already leaving for.
  const here = useRouterState({ select: (state) => (state.resolvedLocation ?? state.location).href })
  const home = useRouterState({ select: (state) => state.location.pathname === '/' })
  const routeBot = useRouterState({ select: (state) => /^\/b\/([^/]+)/.exec(state.location.pathname)?.[1] ?? null })
  const access = accessOf({ ready: Boolean(ready), phase, joining: Boolean(joining), savedEntries: entries.length, invitation: Boolean(initialInvitation), offlineRetryMs: link.kind === 'offline' ? link.retryInMs : null })
  if (access.kind === 'auth') return <Navigate to="/connect" search={here === '/' ? {} : { redirect: here }} replace />
  if (access.kind === 'resuming') return <ShellSkeleton home={home} retry={access.retryInSeconds} onRetry={retryNow} onLeave={disconnect} />
  if (!ready) return <ShellSkeleton home={home} />
  const key = `${ready.principal.network_id}:${ready.principal.device_id}`
  return <WorkspaceProvider key={key} platform={webPlatform} connection={ready} tabs={tabs} initialBot={routeBot && decodeURIComponent(routeBot)} openConversation={(id) => { void navigate({ to: '/', search: { conversation: id } }) }}>
    {onboarding ? <Suspense fallback={<ShellSkeleton />}><Onboarding /></Suspense> : <WorkspaceShell landing={onboarded}><Outlet /></WorkspaceShell>}
  </WorkspaceProvider>
}
