import { createFileRoute, Navigate } from '@tanstack/react-router'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { useDevice } from '@vibeface/client/identity/context.tsx'

export const Route = createFileRoute('/join')({ component: Page })

function Page() {
  const { ready } = useDevice()
  return ready ? <Navigate to="/" replace /> : <ConnectionScreen lane="invite" />
}
