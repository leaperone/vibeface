import { useLocalSearchParams } from 'expo-router'
import { TaskContextScreen } from '@/features/workspace/workspace'

export default function TaskContext() {
  return <TaskContextScreen taskId={useLocalSearchParams<{ taskId: string }>().taskId} />
}
