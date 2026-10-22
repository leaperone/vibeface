import { useEffect } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { returnPath } from '@vibeface/client/identity/access.ts'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { useDevice } from '@vibeface/client/identity/context.tsx'

export const Route = createFileRoute('/connect')({
  validateSearch: (search: Record<string, unknown>): { redirect?: string } => typeof search.redirect === 'string' ? { redirect: search.redirect } : {},
  component: Page,
})

/** Where a browser without a membership joins, recovers or connects; it returns to the page it came from. */
function Page() {
  const { ready } = useDevice()
  const { redirect } = Route.useSearch()
  const navigate = useNavigate()
  useEffect(() => { if (ready) void navigate({ href: returnPath(redirect), replace: true }) }, [ready])
  return ready ? null : <ConnectionScreen />
}
