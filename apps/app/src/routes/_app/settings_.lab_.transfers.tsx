import { createFileRoute } from '@tanstack/react-router'
import { Transfers } from '@/features/lab/transfers'

export const Route = createFileRoute('/_app/settings_/lab_/transfers')({ component: Transfers })
