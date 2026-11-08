import { createFileRoute } from '@tanstack/react-router'
import { DesktopNodeSettings } from '@/features/desktop/desktop-node-settings'

export const Route = createFileRoute('/_app/settings_/desktop-node')({ component: DesktopNodeSettings })
