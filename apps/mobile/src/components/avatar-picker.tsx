import { Button } from '@/components/arc/button'
import { SelectionItem } from '@/components/arc/selection'
import { Eye, LoaderCircle, Palette, RotateCcw, Shapes, type LucideIcon } from 'lucide-react-native'
import { useEffect, type ReactNode } from 'react'
import { AccessibilityInfo, View } from 'react-native'
import { useUniwind } from 'uniwind'
import { colors, EYES, HUES, OUTLINES, type Avatar, type Face } from '@vibeface/client/avatar/face.ts'
import { eyeNames, hueNames, outlineNames, withPart } from '@vibeface/client/avatar/pick.ts'
import { BotFace } from '@/components/bot-face'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { cn } from '@/lib/utils'

/** Outline, eyes and colour as rows of still, tappable previews, and a way back to the face the id gives. Each tap applies at once; while `busy` a pick is being written and the rows wait. */
export function AvatarPicker({ botId, avatar, face, disabled, busy, pick }: { botId: string; avatar: Avatar | null; face: Face; disabled: boolean; busy: boolean; pick: (avatar: Avatar | null) => void }) {
  const { theme } = useUniwind()
  const off = disabled || busy
  // iOS ignores live regions, so the wait is announced on both platforms.
  useEffect(() => { if (busy) AccessibilityInfo.announceForAccessibility('正在换') }, [busy])
  return <View accessibilityState={{ busy }} className="gap-1 self-stretch">
    <Choices icon={Shapes} label="轮廓">
      {OUTLINES.map((outline) => <Choice key={outline} label={outlineNames[outline]} on={face.outline === outline} disabled={off} onPress={() => pick(withPart(face, { outline }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { outline }) }} mood="idle" size={28} still />
      </Choice>)}
    </Choices>
    <Choices icon={Eye} label="眼睛">
      {EYES.map((eyes) => <Choice key={eyes} label={eyeNames[eyes]} on={face.eyes === eyes} disabled={off} onPress={() => pick(withPart(face, { eyes }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { eyes }) }} mood="idle" size={28} still />
      </Choice>)}
    </Choices>
    <Choices icon={Palette} label="颜色">
      {HUES.map((hue) => <Choice key={hue} label={hueNames[hue]} on={face.hue === hue} disabled={off} onPress={() => pick(withPart(face, { hue }))}>
        <View className="size-6 rounded-pill" style={{ backgroundColor: colors({ ...face, hue }, theme === 'dark', 1).base }} />
      </Choice>)}
    </Choices>
    <View className="min-h-11 flex-row items-center gap-2">
      <Button variant="ghost" accessibilityRole="button" disabled={off || !avatar} onPress={() => pick(null)}
        className={cn('min-h-11 flex-row items-center gap-2 rounded-control px-2 active:bg-surface-muted', (off || !avatar) && 'opacity-50')}>
        <Icon as={RotateCcw} className="size-4 text-muted" />
        <Text className="text-sm text-muted">恢复默认</Text>
      </Button>
      {busy ? <View className="flex-row items-center gap-1"><Icon as={LoaderCircle} className="text-muted" /><Text className="text-sm text-muted">正在换</Text></View> : null}
    </View>
  </View>
}

function Choices({ icon, label, children }: { icon: LucideIcon; label: string; children: ReactNode }) {
  return <View accessibilityRole="radiogroup" accessibilityLabel={label} className="flex-row items-start gap-1">
    <View className="h-11 justify-center px-2"><Icon as={icon} className="size-4 text-muted" /></View>
    <View className="min-w-0 flex-1 flex-row flex-wrap gap-0.5">{children}</View>
  </View>
}

function Choice({ label, on, disabled, onPress, children }: { label: string; on: boolean; disabled: boolean; onPress: () => void; children: ReactNode }) {
  return <SelectionItem accessibilityRole="radio" accessibilityLabel={label} accessibilityState={{ checked: on, disabled }} disabled={disabled} onPress={on ? undefined : onPress}
    className={cn('size-11 items-center justify-center rounded-control active:bg-surface-muted', on ? 'border-2 border-accent bg-surface-muted' : disabled && 'opacity-50')}>
    {children}
  </SelectionItem>
}
