import { createFileRoute } from '@tanstack/react-router'
import { TaskDetail } from '@/features/workspace/task-detail'

export const Route = createFileRoute('/_app/b/$botId/work/$workId')({ component: WorkRoute })

function WorkRoute() {
  const { botId, workId } = Route.useParams()
  return <TaskDetail key={workId} botId={botId} workId={workId} />
}
