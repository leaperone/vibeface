import { Lab } from '@/features/lab/lab'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/settings_/lab')({ component: Lab })
