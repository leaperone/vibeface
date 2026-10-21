import { lazy, Suspense } from 'react'
import { Navigate, Outlet, createFileRoute, useNavigate, useRouterState } from '@tanstack/react-router'
import { accessOf } from '@/features/identity/access'
import { useDevice } from '@/features/identity/context'
import { useOnboarding } from '@/features/identity/onboarding-flag'
import { WorkspaceProvider } from '@/features/workspace/context'
import { SESSIONS_SCOPE } from '@/features/workspace/questions'
import { ShellSkeleton, WorkspaceShell } from '@/features/workspace/workspace-shell'

const Onboarding = lazy(() => import('@/features/identity/onboarding').then((module) => ({ default: module.Onboarding })))

export const Route = createFileRoute('/_app')({ component: AppLayout })

/** Workspace pages need a member connection; a browser with saved credentials waits here while it reconnects. */
function AppLayout() {
  const { ready, phase, entries, initialInvitation, tabs, joining, link, disconnect } = useDevice()
  const navigate = useNavigate()
  const onboarding = useOnboarding()
  const here = useRouterState({ select: (state) => state.location.href })
  const home = useRouterState({ select: (state) => state.location.pathname === '/' })
  const access = accessOf({ ready: Boolean(ready), phase, joining: Boolean(joining), savedEntries: entries.length, invitation: Boolean(initialInvitation), offlineRetryMs: link.kind === 'offline' ? link.retryInMs : null })
  if (access.kind === 'auth') return <Navigate to="/connect" search={here === '/' ? {} : { redirect: here }} replace />
  if (access.kind === 'resuming') return <ShellSkeleton home={home} retry={access.retryInSeconds} onLeave={disconnect} />
  if (!ready) return <ShellSkeleton home={home} />
  const key = `${ready.principal.network_id}:${ready.principal.device_id}`
  return <WorkspaceProvider key={key} connection={ready} tabs={tabs} openConversation={(id) => { void (id === SESSIONS_SCOPE ? navigate({ to: '/inbox' }) : navigate({ to: '/', search: { conversation: id } })) }}>
    {onboarding ? <Suspense fallback={<ShellSkeleton />}><Onboarding /></Suspense> : <WorkspaceShell><Outlet /></WorkspaceShell>}
  </WorkspaceProvider>
}
