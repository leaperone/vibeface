import { createFileRoute, Outlet } from '@tanstack/react-router'
import { useEffect } from 'react'
import { useWorkspace } from '@/features/workspace/context'

export const Route = createFileRoute('/_app/b/$botId')({ component: ChatRoute })

/** The conversation itself stays mounted in the shell so drafts survive navigation; this route only selects it. */
function ChatRoute() {
  const { botId } = Route.useParams()
  const workspace = useWorkspace()
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  useEffect(() => { workspace.setView('chat'); return () => workspace.setView('other') }, [])
  useEffect(() => {
    if (bot && bot.conversation_id !== workspace.conversationId) void workspace.run(() => workspace.selectBot(bot))
  }, [bot?.bot_id, workspace.conversationId])
  return <Outlet />
}
