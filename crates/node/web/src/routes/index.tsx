import { createFileRoute } from '@tanstack/react-router'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { DeviceProvider, useDevice } from '@/features/identity/context'
import { WorkspaceProvider } from '@/features/workspace/context'
import { projectScopeKey } from '@/lib/scope'
import { WorkspaceShell } from '@/features/workspace/workspace-shell'

export const Route = createFileRoute('/')({ component: Home })

function Home() {
  return <DeviceProvider><ConnectedWorkspace /></DeviceProvider>
}

function ConnectedWorkspace() {
  const { ready } = useDevice()
  if (!ready) return <ConnectionScreen />
  const key = `${projectScopeKey(ready.scope, ready.project.id, "workspace")}.${ready.generation}`
  return (
    <WorkspaceProvider key={key} connection={ready}>
      <WorkspaceShell />
    </WorkspaceProvider>
  )
}
