import { useElapsed } from '@vibeface/client/workspace/elapsed.ts'
import { Text } from '@/components/arc/text'

/** A task's run time, counting while `active`; `dot` leads it with the separator of the line it joins. */
export function Elapsed({ start, end, active, dot = false }: { start?: number | null; end?: number | null; active: boolean; dot?: boolean }) {
  const label = useElapsed(start, end, active)
  if (!label) return null
  return <Text accessibilityLabel={`用时 ${label}`} className="text-sm text-muted" style={{ fontVariant: ['tabular-nums'] }}>{dot ? `· ${label}` : label}</Text>
}
