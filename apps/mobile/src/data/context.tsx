import { createContext, useContext, type ReactNode } from 'react'
import type { Workspace } from './types'

const WorkspaceContext = createContext<Workspace | null>(null)

export function WorkspaceProvider({ value, children }: { value: Workspace; children: ReactNode }) {
  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>
}

export function useWorkspace(): Workspace {
  const workspace = useContext(WorkspaceContext)
  if (!workspace) throw new Error('useWorkspace outside WorkspaceProvider')
  return workspace
}
