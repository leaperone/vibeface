import { Stack } from 'expo-router'
import { TasksScreen } from '@/features/home/tasks'

export default function Tasks() {
  return <>
    <Stack.Screen options={{ title: '任务' }} />
    <TasksScreen />
  </>
}
