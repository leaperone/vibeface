import { createFileRoute, Link } from '@tanstack/react-router'
import { ChevronLeft } from 'lucide-react'
import { DeviceDetail } from '@/features/devices/devices'

export const Route = createFileRoute('/_app/devices/$deviceId')({ component: DevicePage })

function DevicePage() {
  const { deviceId } = Route.useParams()
  return <div className="grid gap-4">
    <Link to="/devices" className="mx-auto flex w-full max-w-3xl items-center gap-1 text-meta text-muted-foreground hover:text-foreground"><ChevronLeft aria-hidden="true" className="size-4" />我的设备</Link>
    <DeviceDetail key={deviceId} deviceId={deviceId} />
  </div>
}
