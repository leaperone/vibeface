import { ArrowRightLeft, RefreshCw, Wifi, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useRef, useState } from 'react'
import { Modal, Pressable, View, type HostInstance } from 'react-native'
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
    <Text className="text-meta text-muted-foreground">{look.label}</Text>
  </View>
}

/** The connection as a header icon tinted by state; tapping drops the full pill below it. */
export function ConnectionMenu({ connection }: { connection: Connection }) {
  const look = looks[connection.state]
  const button = useRef<HostInstance>(null)
  const [at, setAt] = useState<{ x: number; y: number } | null>(null)
  const close = () => setAt(null)
  return <>
    <Pressable ref={button} onPress={() => button.current?.measureInWindow((x, y, _width, height) => setAt({ x, y: y + height }))}
      accessibilityRole="button" accessibilityLabel={`连接：${look.label}`} accessibilityState={{ expanded: at !== null }} className="size-11 items-center justify-center rounded-full active:bg-muted">
      <Icon as={look.icon} className={cn('size-5', toneText[look.tone])} />
    </Pressable>
    <Modal transparent statusBarTranslucent navigationBarTranslucent visible={at !== null} animationType="fade" onRequestClose={close}>
      <Pressable className="absolute inset-0" onPress={close} accessibilityRole="button" accessibilityLabel="关闭" />
      {at ? <View style={{ position: 'absolute', left: at.x, top: at.y + 4 }} className="w-64 rounded-xl border border-border bg-background px-2 shadow-lg">
        <ConnectionPill connection={connection} />
      </View> : null}
    </Modal>
  </>
}
