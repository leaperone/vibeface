import { ArrowRightLeft, RefreshCw, ShieldCheck, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { Modal, Pressable, useWindowDimensions, View, type HostInstance } from 'react-native'
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

const describe = (connection: Connection) => [connection.entry, connection.state === 'online' ? '已加密' : null, looks[connection.state].label].filter(Boolean).join(' · ')

/** As the web's connection capsule: the entry device by name, a shield once the encrypted link is up. */
export function ConnectionPill({ connection }: { connection: Connection }) {
  const look = looks[connection.state]
  return <View accessible accessibilityLabel={describe(connection)} className="min-h-11 flex-row items-center gap-2 px-2 py-2">
    <Icon as={look.icon} className={cn('size-4', toneText[look.tone])} />
    <Text className="min-w-0 flex-1 text-meta">{connection.entry}</Text>
    <Text className="text-meta text-muted-foreground">{look.label}</Text>
  </View>
}

/** The connection as a header icon tinted by state; tapping drops the full pill below it. */
export function ConnectionMenu({ connection }: { connection: Connection }) {
  const look = looks[connection.state]
  const button = useRef<HostInstance>(null)
  const window = useWindowDimensions()
  const [open, setOpen] = useState(false)
  // Kept after closing, so the card fades out with the modal instead of vanishing under its still-present backdrop.
  const [at, setAt] = useState<{ x: number; y: number } | null>(null)
  // A rotation or resize moves the icon away from where the card was placed.
  useEffect(() => setOpen(false), [window.width, window.height])
  // A little left of and, by its padding, below the large title's first glyph, so no stroke shows past the rounded corner.
  const left = at ? Math.max(8, at.x - 8) : 0
  return <>
    <Pressable ref={button} onPress={() => button.current?.measureInWindow((x, y, _width, height) => { setAt({ x, y: y + height }); setOpen(true) })}
      accessibilityRole="button" accessibilityLabel={`连接：${describe(connection)}`} className="size-11 items-center justify-center rounded-full active:bg-muted">
      <Icon as={look.icon} className={cn('size-5', toneText[look.tone])} />
    </Pressable>
    <Modal transparent statusBarTranslucent navigationBarTranslucent visible={open} animationType="fade" onRequestClose={() => setOpen(false)}>
      <Pressable className="absolute inset-0" onPress={() => setOpen(false)} accessibilityRole="button" accessibilityLabel="关闭" />
      {at ? <View style={{ position: 'absolute', left, top: at.y + 4, width: Math.min(320, window.width - left - 8) }} className="rounded-xl border border-border bg-background px-2 py-1 shadow-lg">
        <ConnectionPill connection={connection} />
      </View> : null}
    </Modal>
  </>
}
