import { createFileRoute, Navigate } from '@tanstack/react-router'
import { ConversationList } from '@/features/workspace/conversation-list'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { homeBot } from '@vibeface/client/workspace/home.ts'
import { useIsMobile } from '@/lib/hooks/use-mobile'

export const Route = createFileRoute('/_app/')({
  validateSearch: (search: Record<string, unknown>): { conversation?: string; work?: string; routine?: string } => ({ ...(typeof search.conversation === 'string' ? { conversation: search.conversation } : {}), ...(typeof search.work === 'string' ? { work: search.work } : {}), ...(typeof search.routine === 'string' ? { routine: search.routine } : {}) }),
  component: Home,
})

function Home() {
  const workspace = useWorkspace()
  const { conversation, work, routine } = Route.useSearch()
  const bot = homeBot({ narrow: useIsMobile(), conversation, bots: workspace.bots, current: workspace.conversationId })
  return bot ? work ? <Navigate to="/b/$botId/work/$workId" params={{ botId: bot.bot_id, workId: work }} replace /> : <Navigate to="/b/$botId" params={{ botId: bot.bot_id }} search={routine ? { routine } : {}} replace /> : <ConversationList />
}
