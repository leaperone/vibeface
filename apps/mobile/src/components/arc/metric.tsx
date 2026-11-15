import { Pressable } from 'react-native'
import { Text } from './text'
import { cn } from '@/lib/utils'

/** Native metric-card port: a tabular count with its label and selected state. */
export function Metric({ label, value, tone, selected, onPress }: { label: string; value: number | null; tone: 'neutral' | 'warning' | 'info'; selected: boolean; onPress(): void }) {
  return <Pressable accessibilityRole="button" accessibilityState={{ selected }} accessibilityLabel={`${label} ${value ?? '待确认'}`} onPress={onPress}
    className={cn('min-h-20 min-w-0 flex-1 gap-2 rounded-panel border p-3 active:bg-surface-muted', selected ? 'border-accent bg-accent-subtle' : 'border-border bg-surface')}>
    <Text className="text-sm text-secondary">{label}</Text>
    <Text className={cn('text-2xl font-normal', tone === 'warning' && value ? 'text-warning' : tone === 'info' && value ? 'text-info' : 'text-foreground')} style={{ fontVariant: ['tabular-nums'] }}>{value ?? '待确认'}</Text>
  </Pressable>
}
