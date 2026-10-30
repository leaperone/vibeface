import { createFileRoute } from "@tanstack/react-router"
import { TasksOverview } from "@/features/workspace/tasks-overview"

export const Route = createFileRoute("/_app/tasks")({ component: TasksOverview })
