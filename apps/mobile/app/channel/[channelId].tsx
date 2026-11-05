import { useLocalSearchParams } from 'expo-router'
import { ScopedWorkspaceScreen } from '@/features/workspace/workspace'

export default function Channel() {
  const { channelId } = useLocalSearchParams<{ channelId: string }>()
  return <ScopedWorkspaceScreen key={channelId} channelId={channelId} />
}
