import { createFileRoute } from '@tanstack/react-router'
import { WorkspaceTaskDetail } from '@/features/workspace/workspace-chat'

export const Route = createFileRoute('/_app/tasks_/$taskId')({ component: TaskRoute })

function TaskRoute() {
  const { taskId } = Route.useParams()
  return <WorkspaceTaskDetail key={taskId} taskId={taskId} />
}
