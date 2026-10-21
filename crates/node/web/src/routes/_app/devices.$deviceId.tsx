import { createFileRoute } from '@tanstack/react-router'
import { DeviceDetail } from '@/features/devices/devices'

export const Route = createFileRoute('/_app/devices/$deviceId')({ component: DevicePage })

function DevicePage() {
  const { deviceId } = Route.useParams()
  return <DeviceDetail key={deviceId} deviceId={deviceId} />
}
