import { useState } from 'react'
import { createFileRoute, Navigate, useNavigate } from '@tanstack/react-router'
import { ArrowRightLeft, Crown } from 'lucide-react'
import { AuthPage, InlineError, Panel } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { ConnectionScreen } from '@/features/identity/connection-screen'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { onlyManagerText, soleManager, switchCopy } from '@vibeface/client/identity/copy.ts'

export const Route = createFileRoute('/join')({ component: Page })

function Page() {
  const { ready, initialInvitation, linkTarget } = useDevice()
  if (!ready) return <ConnectionScreen lane="invite" />
  return initialInvitation && linkTarget(initialInvitation) === 'other' ? <SwitchNetwork /> : <Navigate to="/" replace />
}

/** A join link from another network: this browser leaves its own, then the join page below picks the same link up. */
function SwitchNetwork() {
  const device = useDevice()
  const navigate = useNavigate()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<unknown>(null)
  // The network keeps at least one manager, so the last one cannot leave; a local session only disconnects.
  const sole = device.ready?.principal.source !== 'local_session' && soleManager(device.directory?.members ?? [], device.identity?.deviceId)
  const home = () => void navigate({ to: '/', replace: true })
  return <AuthPage>
    <Panel icon={ArrowRightLeft} title={switchCopy.title}>
      {sole
        ? <p className="flex items-start gap-2"><Crown aria-hidden="true" className="mt-1 size-4 shrink-0 text-warning" />{onlyManagerText('这个浏览器')}</p>
        : <p className="text-muted-foreground">{switchCopy.leave('这个浏览器')}</p>}
      <InlineError error={error} />
      <div className="flex justify-end gap-2">
        <Button type="button" variant="ghost" onClick={home}>{sole ? '知道了' : '取消'}</Button>
        {sole ? null : <Button type="button" variant="destructive" disabled={busy} onClick={() => {
          setBusy(true); setError(null)
          device.leave().catch(setError).finally(() => setBusy(false))
        }}><ArrowRightLeft aria-hidden="true" />{switchCopy.confirm}</Button>}
      </div>
    </Panel>
  </AuthPage>
}
