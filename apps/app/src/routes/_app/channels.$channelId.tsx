import { ChannelWorkspace } from '@/features/workspace/workspace-chat'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/channels/$channelId')({ component: ChannelRoute })

function ChannelRoute() {
  const { channelId } = Route.useParams()
  return <ChannelWorkspace key={channelId} channelId={channelId} />
}
