import { createFileRoute } from '@tanstack/react-router'
import { ChannelsOverview } from '@/features/workspace/channels'

export const Route = createFileRoute('/_app/channels/')({ component: ChannelsOverview })
