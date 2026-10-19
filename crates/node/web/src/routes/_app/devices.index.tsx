import { createFileRoute } from '@tanstack/react-router'
import { AddDevice } from '@/features/devices/add-device'
import { DevicesPage } from '@/features/devices/devices'

export const Route = createFileRoute('/_app/devices/')({ component: () => <DevicesPage actions={<AddDevice />} /> })
