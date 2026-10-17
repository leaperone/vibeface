import { createFileRoute } from '@tanstack/react-router'
import { useState } from 'react'

import { ConversationScreen } from '@/features/chat'
import { logout, type Session } from '@/lib/node'
import { AuthScreen, WorkspaceShell } from '@/features/workspace'

export const Route = createFileRoute('/')({ component: Home })

function Home() {
  const [session, setSession] = useState<Session | null>(null)
  if (!session) return <AuthScreen onSession={setSession} />
  return (
    <WorkspaceShell
      session={session}
      onLogout={() => {
        void logout().finally(() => setSession(null))
      }}
      chat={(slot) => <ConversationScreen {...slot} session={session} />}
    />
  )
}
