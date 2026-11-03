import { useState } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { AddDevice } from '@/features/devices/add-device'
import { DevicesPage } from '@/features/devices/devices'
import { botSearch } from '@vibeface/client/workspace/home.ts'

export const Route = createFileRoute('/_app/devices/')({
  validateSearch: (search: Record<string, unknown>): { add?: true | 'computer'; manage?: true; bot?: string } => ({ ...(search.add === true || search.add === 'true' ? { add: true as const } : search.add === 'computer' ? { add: 'computer' as const } : {}), ...(search.manage === true || search.manage === 'true' ? { manage: true as const } : {}), ...botSearch(search) }),
  component: Devices,
})

function Devices() {
  const { add, manage, bot } = Route.useSearch()
  const navigate = useNavigate()
  // A waiting request shows in one place: the sheet while it is open, the page otherwise.
  const [adding, setAdding] = useState(false)
  return <><DevicesPage joins={!adding} /><AddDevice onOpenChange={setAdding} opening={add === 'computer' ? 'computer' : add === true ? 'phone' : undefined} manage={manage} onOpened={() => { void navigate({ to: '/devices', search: { bot }, replace: true }) }} /></>
}
