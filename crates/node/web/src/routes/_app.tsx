import { Outlet, createFileRoute, useNavigate } from '@tanstack/react-router'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { useDevice } from '@/features/identity/context'
import { WorkspaceProvider } from '@/features/workspace/context'
import { WorkspaceShell } from '@/features/workspace/workspace-shell'

export const Route = createFileRoute('/_app')({ component: AppLayout })

function AppLayout() {
  const { ready } = useDevice()
  const navigate = useNavigate()
  if (!ready) return <ConnectionScreen />
  const key = `${ready.principal.network_id}:${ready.principal.subject_id}`
  return <WorkspaceProvider key={key} connection={ready} openConversation={(id) => { void navigate({ to: '/', search: { conversation: id } }) }}>
    <WorkspaceShell><Outlet /></WorkspaceShell>
  </WorkspaceProvider>
}
