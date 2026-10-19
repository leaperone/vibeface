import { createFileRoute } from '@tanstack/react-router'
import { DeviceControls } from '@/features/identity/device-controls'
import { NodeSettingsControls } from '@/features/workspace/node-settings'

export const Route = createFileRoute('/_app/devices/')({ component: DevicesPage })

function DevicesPage() {
  return <div className="mx-auto grid max-w-3xl gap-6">
    <NodeSettingsControls active />
    <DeviceControls active />
  </div>
}
