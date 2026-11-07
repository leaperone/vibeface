import { createFileRoute, Outlet, useRouterState } from '@tanstack/react-router'
import { useEffect } from 'react'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'

export const Route = createFileRoute('/_app/b/$botId')({ component: ChatRoute })

/** The conversation itself stays mounted in the shell so drafts survive navigation; this route only selects it. */
function ChatRoute() {
  const { botId } = Route.useParams()
  const workspace = useWorkspace()
  const bot = workspace.bots.find((item) => item.bot_id === botId)
  const visible = useRouterState({ select: (state) => /^\/b\/[^/]+\/?$/.test(state.location.pathname) })
  useEffect(() => {
    let current = true
    workspace.setView('other')
    if (bot) void workspace.run(async () => {
      await workspace.selectBot(bot)
      if (current && workspace.isCurrent()) workspace.setView(visible ? 'chat' : 'other')
    })
    return () => { current = false; if (workspace.isCurrent()) workspace.setView('other') }
  }, [bot?.bot_id, workspace.generation, visible])
  return <Outlet />
}
