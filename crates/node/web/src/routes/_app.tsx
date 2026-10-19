import { Outlet, createFileRoute, useNavigate } from '@tanstack/react-router'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { useDevice } from '@/features/identity/context'
import { Onboarding } from '@/features/identity/onboarding'
import { useOnboarding } from '@/features/identity/onboarding-flag'
import { WorkspaceProvider } from '@/features/workspace/context'
import { ShellSkeleton, WorkspaceShell } from '@/features/workspace/workspace-shell'

export const Route = createFileRoute('/_app')({ component: AppLayout })

function AppLayout() {
  const { ready, phase, entries, initialInvitation } = useDevice()
  const navigate = useNavigate()
  const onboarding = useOnboarding()
  if (!ready && (phase === 'initializing' || phase === 'connecting' && entries.length > 0 && !initialInvitation)) return <ShellSkeleton />
  if (!ready) return <ConnectionScreen />
  const key = `${ready.principal.network_id}:${ready.principal.subject_id}`
  return <WorkspaceProvider key={key} connection={ready} openConversation={(id) => { void navigate({ to: '/', search: { conversation: id } }) }}>
    {onboarding ? <Onboarding /> : <WorkspaceShell><Outlet /></WorkspaceShell>}
  </WorkspaceProvider>
}
