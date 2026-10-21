import { createFileRoute, Navigate } from '@tanstack/react-router'
import { Bot, Plus } from 'lucide-react'
import { EmptyState, Skeleton } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { useWorkspace } from '@/features/workspace/context'
import { useCreateBot } from '@/features/workspace/workspace-shell'

export const Route = createFileRoute('/_app/')({
  validateSearch: (search: Record<string, unknown>): { conversation?: string } => typeof search.conversation === 'string' ? { conversation: search.conversation } : {},
  component: Home,
})

/** `/` opens the assistant from a notification, the last one used, or the first one. */
function Home() {
  const workspace = useWorkspace()
  const create = useCreateBot()
  const { conversation } = Route.useSearch()
  const bot = workspace.bots.find((item) => item.conversation_id === conversation)
    ?? workspace.bots.find((item) => item.conversation_id === workspace.conversationId)
    ?? workspace.bots[0]
  if (bot) return <Navigate to="/b/$botId" params={{ botId: bot.bot_id }} replace />
  if (!workspace.joined) return <EmptyState icon={Bot} title="还没有助手" />
  if (!workspace.writable && !workspace.bots.length) return <div className="mx-auto grid max-w-md gap-3 py-16"><Skeleton className="mx-auto size-8" /><Skeleton className="mx-auto h-5 w-40" /></div>
  return <EmptyState icon={Bot} title="还没有助手" action={<Button type="button" onClick={create}><Plus aria-hidden="true" />新建助手</Button>} />
}
