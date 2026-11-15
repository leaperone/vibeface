import { Transfers } from '@/features/lab/transfers'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/settings_/lab_/transfers')({ component: Transfers })
