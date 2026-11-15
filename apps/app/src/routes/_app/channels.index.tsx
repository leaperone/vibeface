import { ChannelsOverview } from '@/features/workspace/channels'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/channels/')({ component: ChannelsOverview })
