import { createFileRoute } from '@tanstack/react-router'
import { Lab } from '@/features/lab/lab'

export const Route = createFileRoute('/_app/settings_/lab')({ component: Lab })
