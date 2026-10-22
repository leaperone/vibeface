import { ArrowRightLeft, RefreshCw, Wifi, WifiOff, type LucideIcon } from 'lucide-react-native'
import { View } from 'react-native'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { toneText, type Tone } from '@/components/kit'
import type { Connection } from '@/data/types'
import { cn } from '@/lib/utils'

const looks: Record<Connection['state'], { label: string; icon: LucideIcon; tone: Tone }> = {
  connecting: { label: '连接中', icon: ArrowRightLeft, tone: 'info' },
  syncing: { label: '同步中', icon: RefreshCw, tone: 'info' },
  offline: { label: '离线', icon: WifiOff, tone: 'neutral' },
  online: { label: '在线', icon: Wifi, tone: 'success' },
}

export function ConnectionPill({ connection }: { connection: Connection }) {
  const look = looks[connection.state]
  return <View accessible accessibilityLabel={`经 ${connection.entry} · ${look.label}`} className="min-h-11 flex-row items-center gap-2 px-2">
    <Icon as={look.icon} className={cn('size-4', toneText[look.tone])} />
    <Text numberOfLines={1} className="min-w-0 flex-1 text-meta">经 {connection.entry}</Text>
    <Text className={cn('text-meta', toneText[look.tone])}>{look.label}</Text>
  </View>
}
