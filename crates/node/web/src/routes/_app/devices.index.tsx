import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { AddDevice } from '@/features/devices/add-device'
import { DevicesPage } from '@/features/devices/devices'

export const Route = createFileRoute('/_app/devices/')({
  validateSearch: (search: Record<string, unknown>): { add?: true } => search.add === true || search.add === 'true' ? { add: true } : {},
  component: Devices,
})

function Devices() {
  const { add } = Route.useSearch()
  const navigate = useNavigate()
  return <DevicesPage actions={<AddDevice opening={add === true} onOpened={() => { void navigate({ to: '/devices', search: {}, replace: true }) }} />} />
}
