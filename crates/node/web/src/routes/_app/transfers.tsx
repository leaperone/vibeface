import { createFileRoute } from '@tanstack/react-router'
import { Transfers } from '@/features/transfers/transfers'

export const Route = createFileRoute('/_app/transfers')({ component: Transfers })
