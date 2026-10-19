import { lazy, Suspense } from 'react'
import { Outlet, createFileRoute, useNavigate } from '@tanstack/react-router'
import { useDevice } from '@/features/identity/context'
import { useOnboarding } from '@/features/identity/onboarding-flag'
import { WorkspaceProvider } from '@/features/workspace/context'
import { ShellSkeleton, WorkspaceShell } from '@/features/workspace/workspace-shell'

// Setup, pairing and recovery only load for browsers that aren't connected yet.
const ConnectionScreen = lazy(() => import('@/features/identity/connection-screen').then((module) => ({ default: module.ConnectionScreen })))
const Onboarding = lazy(() => import('@/features/identity/onboarding').then((module) => ({ default: module.Onboarding })))

export const Route = createFileRoute('/_app')({ component: AppLayout })

function AppLayout() {
  const { ready, phase, entries, initialInvitation } = useDevice()
  const navigate = useNavigate()
  const onboarding = useOnboarding()
  if (!ready && (phase === 'initializing' || phase === 'connecting' && entries.length > 0 && !initialInvitation)) return <ShellSkeleton />
  if (!ready) return <Suspense fallback={<ShellSkeleton />}><ConnectionScreen /></Suspense>
  const key = `${ready.principal.network_id}:${ready.principal.subject_id}`
  return <WorkspaceProvider key={key} connection={ready} openConversation={(id) => { void navigate({ to: '/', search: { conversation: id } }) }}>
    {onboarding ? <Suspense fallback={<ShellSkeleton />}><Onboarding /></Suspense> : <WorkspaceShell><Outlet /></WorkspaceShell>}
  </WorkspaceProvider>
}
