import { createFileRoute, Navigate } from '@tanstack/react-router'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { useDevice } from '@/features/identity/context'

export const Route = createFileRoute('/setup')({ component: Page })

function Page() {
  const { ready } = useDevice()
  return ready ? <Navigate to="/" replace /> : <ConnectionScreen />
}
