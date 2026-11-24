import { WorkspaceTaskDetail } from '@/features/workspace/workspace-chat'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/tasks_/$taskId')({ validateSearch: (search: Record<string, unknown>): { message?: string } => typeof search.message === 'string' ? { message: search.message } : {}, component: TaskRoute })

function TaskRoute() {
  const { taskId } = Route.useParams()
  return <WorkspaceTaskDetail key={taskId} taskId={taskId} />
}
