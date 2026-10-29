import { Eye, Palette, RotateCcw, Shapes, type LucideIcon } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { useUniwind } from 'uniwind'
import { colors, EYES, HUES, OUTLINES, type Avatar, type Face } from '@vibeface/client/avatar/face.ts'
import { eyeNames, hueNames, outlineNames, withPart } from '@vibeface/client/avatar/pick.ts'
import { BotFace } from '@/components/bot-face'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { cn } from '@/lib/utils'

/** Outline, eyes and colour as rows of tappable previews, and a way back to the face the id gives. Each tap applies at once. */
export function AvatarPicker({ botId, avatar, face, disabled, pick }: { botId: string; avatar: Avatar | null; face: Face; disabled: boolean; pick: (avatar: Avatar | null) => void }) {
  const { theme } = useUniwind()
  return <View className="gap-1 self-stretch">
    <Choices icon={Shapes} label="轮廓">
      {OUTLINES.map((outline) => <Choice key={outline} label={outlineNames[outline]} on={face.outline === outline} disabled={disabled} onPress={() => pick(withPart(face, { outline }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { outline }) }} mood="idle" size={28} />
      </Choice>)}
    </Choices>
    <Choices icon={Eye} label="眼睛">
      {EYES.map((eyes) => <Choice key={eyes} label={eyeNames[eyes]} on={face.eyes === eyes} disabled={disabled} onPress={() => pick(withPart(face, { eyes }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { eyes }) }} mood="idle" size={28} />
      </Choice>)}
    </Choices>
    <Choices icon={Palette} label="颜色">
      {HUES.map((hue) => <Choice key={hue} label={hueNames[hue]} on={face.hue === hue} disabled={disabled} onPress={() => pick(withPart(face, { hue }))}>
        <View className="size-6 rounded-full" style={{ backgroundColor: colors({ ...face, hue }, theme === 'dark', 1).base }} />
      </Choice>)}
    </Choices>
    <Pressable accessibilityRole="button" disabled={disabled || !avatar} onPress={() => pick(null)}
      className={cn('min-h-11 flex-row items-center gap-2 self-start rounded-lg px-2 active:bg-muted', (disabled || !avatar) && 'opacity-50')}>
      <Icon as={RotateCcw} className="size-4 text-muted-foreground" />
      <Text className="text-meta text-muted-foreground">恢复默认</Text>
    </Pressable>
  </View>
}

function Choices({ icon, label, children }: { icon: LucideIcon; label: string; children: ReactNode }) {
  return <View accessibilityRole="radiogroup" accessibilityLabel={label} className="flex-row items-start gap-1">
    <View className="h-11 justify-center px-2"><Icon as={icon} className="size-4 text-muted-foreground" /></View>
    <View className="min-w-0 flex-1 flex-row flex-wrap gap-0.5">{children}</View>
  </View>
}

function Choice({ label, on, disabled, onPress, children }: { label: string; on: boolean; disabled: boolean; onPress: () => void; children: ReactNode }) {
  return <Pressable accessibilityRole="radio" accessibilityLabel={label} accessibilityState={{ checked: on, disabled }} disabled={disabled} onPress={on ? undefined : onPress}
    className={cn('size-11 items-center justify-center rounded-lg active:bg-muted', on ? 'border-2 border-brand bg-muted' : disabled && 'opacity-50')}>
    {children}
  </Pressable>
}
