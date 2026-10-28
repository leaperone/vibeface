import { ArrowRightLeft, RefreshCw, ShieldCheck, WifiOff, type LucideIcon } from 'lucide-react-native'
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
  online: { label: '在线', icon: ShieldCheck, tone: 'success' },
}

/** As the web's connection capsule: the entry device by name, a shield once the encrypted link is up. */
export function ConnectionPill({ connection }: { connection: Connection }) {
  const look = looks[connection.state]
  return <View accessible accessibilityLabel={[connection.entry, connection.state === 'online' ? '已加密' : null, look.label].filter(Boolean).join(' · ')} className="min-h-11 flex-row items-center gap-2 px-2">
    <Icon as={look.icon} className={cn('size-4', toneText[look.tone])} />
    <Text numberOfLines={1} className="min-w-0 flex-1 text-meta">{connection.entry}</Text>
    <Text className="text-meta text-muted-foreground">{look.label}</Text>
  </View>
}
