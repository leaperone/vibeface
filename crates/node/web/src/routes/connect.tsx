import { useEffect } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { returnPath } from '@vibeface/client/identity/access.ts'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { justRecovered } from '@/features/identity/recovery-controls'
import { useDevice } from '@vibeface/client/identity/context.tsx'

export const Route = createFileRoute('/connect')({
  validateSearch: (search: Record<string, unknown>): { redirect?: string } => typeof search.redirect === 'string' ? { redirect: search.redirect } : {},
  component: Page,
})

/** Where a browser without a membership joins, recovers or connects; it returns to the page it came from, or after a recovery to its result in settings. */
function Page() {
  const { ready } = useDevice()
  const { redirect } = Route.useSearch()
  const navigate = useNavigate()
  useEffect(() => { if (ready) void navigate({ href: justRecovered() ? '/settings' : returnPath(redirect), replace: true }) }, [ready])
  return ready ? null : <ConnectionScreen />
}
