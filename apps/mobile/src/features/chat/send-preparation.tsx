import { View } from 'react-native'
import { Text } from '@/components/arc/text'

export type SendPreparationState =
  | { kind: 'preparing'; completed?: number; total?: number }
  | { kind: 'saving' }
  | { kind: 'failed'; message: string }

/** This is local preparation feedback; only the caller's durable-save result can clear the draft. */
export function SendPreparation({ state }: { state: SendPreparationState | null }) {
  if (!state) return null
  const text = state.kind === 'failed' ? state.message : state.kind === 'saving' ? '正在本地保存消息，完成后再离开。' : state.total !== undefined && state.completed !== undefined ? `正在准备附件 ${state.completed} / ${state.total}，草稿已保留。` : '正在准备附件，草稿已保留。'
  return <View accessible accessibilityLiveRegion="polite" accessibilityRole={state.kind === 'failed' ? 'alert' : undefined}><Text className={state.kind === 'failed' ? 'text-sm text-danger' : 'text-sm text-muted'}>{text}</Text></View>
}
