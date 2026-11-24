import { TaskDetail } from '@/features/workspace/task-detail'
import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/b/$botId/work/$workId')({
  validateSearch: (search: Record<string, unknown>): { from?: 'tasks'; message?: string } => ({ ...(search.from === 'tasks' ? { from: 'tasks' } : {}), ...(typeof search.message === 'string' ? { message: search.message } : {}) }),
  component: WorkRoute,
})

function WorkRoute() {
  const { botId, workId } = Route.useParams()
  const { from, message } = Route.useSearch()
  return <TaskDetail key={`${botId}:${workId}`} botId={botId} workId={workId} from={from} focus={message} />
}
