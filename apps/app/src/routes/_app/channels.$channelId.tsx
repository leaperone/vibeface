import { ChannelWorkspace } from '@/features/workspace/workspace-chat'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/channels/$channelId')({ validateSearch: (search: Record<string, unknown>): { message?: string } => typeof search.message === 'string' ? { message: search.message } : {}, component: ChannelRoute })

function ChannelRoute() {
  const { channelId } = Route.useParams()
  return <ChannelWorkspace key={channelId} channelId={channelId} />
}
