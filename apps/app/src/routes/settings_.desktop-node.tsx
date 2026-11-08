import { createFileRoute } from '@tanstack/react-router'
import { DesktopNodeSettings } from '@/features/desktop/desktop-node-settings'

/** Desktop node panel: must render without workspace membership or a running Core. */
export const Route = createFileRoute('/settings_/desktop-node')({ component: DesktopNodeSettings })
