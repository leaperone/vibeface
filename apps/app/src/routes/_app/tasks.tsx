import { TasksOverview } from "@/features/workspace/tasks-overview"
import { createFileRoute } from "@tanstack/react-router"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { useEffect } from "react"

function TasksRoute() {
  const workspace = useWorkspace()
  useEffect(() => { workspace.setView("tasks"); return () => workspace.setView("other") }, [workspace.generation])
  return <TasksOverview />
}

export const Route = createFileRoute("/_app/tasks")({ component: TasksRoute })
