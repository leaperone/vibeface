import { useLocalSearchParams } from 'expo-router'
import { ScopedWorkspaceScreen } from '@/features/workspace/workspace'

export default function Task() {
  const { taskId } = useLocalSearchParams<{ taskId: string }>()
  return <ScopedWorkspaceScreen key={taskId} taskId={taskId} />
}
