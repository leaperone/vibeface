import { createFileRoute } from '@tanstack/react-router'
import { DeviceDetail } from '@/features/devices/devices'
import { botSearch } from '@vibeface/client/workspace/home.ts'

export const Route = createFileRoute('/_app/devices/$deviceId')({ validateSearch: botSearch, component: DevicePage })

function DevicePage() {
  const { deviceId } = Route.useParams()
  return <DeviceDetail key={deviceId} deviceId={deviceId} />
}
