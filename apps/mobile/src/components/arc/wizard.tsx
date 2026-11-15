import type { LucideIcon } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { View } from 'react-native'
import { Icon } from './icon'
import { Text } from './text'

/** Arc stepper's current-stage heading and body, without inventing progress for unknown network work. */
export function WizardStep({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-6">
    <View accessibilityRole="header" className="flex-row items-center gap-3">
      <Icon as={icon} className="size-5 text-secondary" />
      <Text role="heading" className="min-w-0 flex-1 text-lg font-medium">{title}</Text>
    </View>
    <View className="gap-4">{children}</View>
  </View>
}
