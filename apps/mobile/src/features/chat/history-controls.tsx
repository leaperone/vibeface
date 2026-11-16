import { useEffect, useRef, useState } from 'react'
import { View } from 'react-native'
import type { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { historySections } from '@vibeface/client/workspace/model.ts'
import { historyLabels } from '@vibeface/client/workspace/history.ts'
import type { HistorySection } from '@vibeface/client/workspace/types.ts'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { Button } from '@/components/arc/button'
import { Text } from '@/components/arc/text'
import { ErrorText } from '@/components/action'

type HistoryReader = Pick<ReturnType<typeof useWorkspace>, 'generation' | 'conversationId' | 'data' | 'historyLoading' | 'connectionOnline' | 'loadHistory' | 'cancelHistory'>
export function HistoryControls({ workspace, sections = historySections }: { workspace: HistoryReader; sections?: HistorySection[] }) {
  const [error, setError] = useState('')
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => {
    const generation = workspace.generation, id = workspace.conversationId
    return () => {
      if (latest.current.generation !== generation || latest.current.conversationId !== id) return
      for (const section of sections) latest.current.cancelHistory(section)
    }
  }, [workspace.generation, workspace.conversationId])
  const missing = sections.filter((section) => workspace.data && !workspace.data.history[section].complete)
  if (!missing.length) return null
  return <View className="gap-2" accessibilityLabel="尚未读取的记录">
    <Text className="text-sm text-muted">部分记录尚未读取，可以按需继续加载。</Text>
    {missing.map((section) => workspace.historyLoading.has(section)
      ? <Button key={section} variant="secondary" onPress={() => workspace.cancelHistory(section)}><Text>取消读取{historyLabels[section]}</Text></Button>
      : <Button key={section} variant="secondary" disabled={!workspace.connectionOnline} onPress={() => { setError(''); void workspace.loadHistory(section).catch((caught) => setError(messageOf(caught))) }}><Text>读取更多{historyLabels[section]}</Text></Button>)}
    {error ? <ErrorText error={error} /> : null}
  </View>
}
