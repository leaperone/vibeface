import { createFileRoute } from '@tanstack/react-router'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { DeviceProvider, useDevice } from '@/features/identity/context'
import { WorkspaceProvider } from '@/features/workspace/context'
import { WorkspaceShell } from '@/features/workspace/workspace-shell'

export const Route = createFileRoute('/')({ component: Home })

function Home() {
  return <DeviceProvider><ConnectedWorkspace /></DeviceProvider>
}

function ConnectedWorkspace() {
  const { ready } = useDevice()
  if (!ready) return <ConnectionScreen />
  const key = `${ready.principal.network_id}:${ready.principal.subject_id}`
  return (
    <WorkspaceProvider key={key} connection={ready}>
      <WorkspaceShell />
    </WorkspaceProvider>
  )
}
