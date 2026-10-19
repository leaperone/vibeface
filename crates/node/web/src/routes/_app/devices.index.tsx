import { createFileRoute } from '@tanstack/react-router'
import { DevicesPage } from '@/features/devices/devices'
import { DeviceControls } from '@/features/identity/device-controls'

export const Route = createFileRoute('/_app/devices/')({ component: Devices })

function Devices() {
  return <div className="grid gap-6">
    <DevicesPage />
    <div className="mx-auto grid w-full max-w-3xl"><DeviceControls active /></div>
  </div>
}
