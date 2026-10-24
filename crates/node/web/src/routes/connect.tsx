import { useEffect } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { Wordmark } from '@/components/app/kit'
import { returnPath } from '@vibeface/client/identity/access.ts'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { RecoveredStep, useJustRecovered } from '@/features/identity/recovery-controls'
import { useDevice } from '@vibeface/client/identity/context.tsx'

export const Route = createFileRoute('/connect')({
  validateSearch: (search: Record<string, unknown>): { redirect?: string } => typeof search.redirect === 'string' ? { redirect: search.redirect } : {},
  component: Page,
})

/** Where a browser without a membership joins, recovers or connects; it returns to the page it came from, after a recovery once its new file is saved. */
function Page() {
  const { ready } = useDevice()
  const recovered = useJustRecovered()
  const { redirect } = Route.useSearch()
  const navigate = useNavigate()
  useEffect(() => { if (ready && !recovered) void navigate({ href: returnPath(redirect), replace: true }) }, [ready, recovered])
  if (ready && recovered) return <main className="h-[var(--app-height,100dvh)] overflow-y-auto overscroll-contain">
    <div className="mx-auto grid w-full max-w-md content-start gap-6 px-4 py-10 pb-[max(2.5rem,env(safe-area-inset-bottom))] sm:min-h-full sm:content-center sm:py-16">
      <header><Wordmark /></header>
      <RecoveredStep />
    </div>
  </main>
  return ready ? null : <ConnectionScreen />
}
