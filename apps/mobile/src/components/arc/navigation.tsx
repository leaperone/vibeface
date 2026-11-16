import { Link, type Href } from 'expo-router'
import type { LucideIcon } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { Icon } from './icon'
import { Text } from './text'
import { cn } from '@/lib/utils'

export type NavigationItem = { href: Href; label: string; icon: LucideIcon; selected: boolean; notice?: ReactNode; accessibilityLabel?: string }
/** Native route tabs port Arc tabs' quiet rail and selected accent treatment. */
export function NavigationTabs({ items, bottom }: { items: NavigationItem[]; bottom: number }) {
  return <View accessibilityRole="tabbar" className="flex-row gap-1 border-t border-border bg-surface px-3 pt-2" style={{ paddingBottom: bottom }}>
    {items.map((item) => <Link key={item.label} href={item.href} replace asChild>
      <Pressable role="button" accessibilityLabel={item.accessibilityLabel ?? item.label} accessibilityState={{ selected: item.selected }} className="min-h-14 flex-1 items-center justify-center gap-1 rounded-control py-2 active:bg-surface-muted">
        <View className={cn('flex-row items-center gap-1', item.selected && 'border-b-2 border-accent pb-1')}>
          <Icon as={item.icon} className={cn('size-5', item.selected ? 'text-accent' : 'text-muted')} />
          {item.notice}
        </View>
        <Text className={cn('text-xs font-medium', item.selected ? 'text-foreground' : 'text-muted')}>{item.label}</Text>
      </Pressable>
    </Link>)}
  </View>
}
/** Arc sidebar's 44px rows, section rhythm and muted rail, using real native routes. */
export function NavigationRail({ items }: { items: NavigationItem[] }) {
  return <View accessibilityRole="menu" className="gap-1 px-3 pb-4">
    {items.map((item) => <Link key={item.label} href={item.href} replace asChild>
      <Pressable accessibilityRole="link" accessibilityLabel={item.accessibilityLabel ?? item.label} accessibilityState={{ selected: item.selected }} className={cn('min-h-11 flex-row items-center gap-3 rounded-control px-3 py-3 active:bg-surface-muted', item.selected && 'bg-accent-subtle')}>
        <Icon as={item.icon} className={item.selected ? 'text-accent' : 'text-secondary'} />
        <Text className={cn('flex-1 text-sm font-medium', item.selected ? 'text-foreground' : 'text-secondary')}>{item.label}</Text>
        {item.notice}
      </Pressable>
    </Link>)}
  </View>
}
