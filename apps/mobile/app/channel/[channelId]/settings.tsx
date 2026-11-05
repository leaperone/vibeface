import { useLocalSearchParams } from 'expo-router'
import { ChannelSettingsScreen } from '@/features/workspace/workspace'

export default function ChannelSettings() {
  return <ChannelSettingsScreen channelId={useLocalSearchParams<{ channelId: string }>().channelId} />
}
