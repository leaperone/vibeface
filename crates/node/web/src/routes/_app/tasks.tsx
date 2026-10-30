import { createFileRoute } from "@tanstack/react-router"
import { useEffect } from "react"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { TasksOverview } from "@/features/workspace/tasks-overview"

function TasksRoute() {
  const workspace = useWorkspace()
  useEffect(() => { workspace.setView("tasks"); return () => workspace.setView("other") }, [workspace.generation])
  return <TasksOverview />
}

export const Route = createFileRoute("/_app/tasks")({ component: TasksRoute })
