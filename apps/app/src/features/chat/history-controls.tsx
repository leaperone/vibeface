import { InlineError } from '@/components/arc/product-kit'
import type { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { historyLabels } from '@vibeface/client/workspace/history.ts'
import { historySections } from '@vibeface/client/workspace/model.ts'
import type { HistorySection } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { useEffect,useRef,useState } from 'react'

type HistoryReader = Pick<ReturnType<typeof useWorkspace>, 'generation' | 'conversationId' | 'data' | 'historyLoading' | 'connectionOnline' | 'loadHistory' | 'cancelHistory'>
export function HistoryControls({ workspace, sections = historySections }: { workspace: HistoryReader; sections?: HistorySection[] }) {
  const [error, setError] = useState<unknown>(null)
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
  return <div className="grid gap-2" aria-label="尚未读取的记录">
    <p className="text-sm text-secondary">部分记录尚未读取，可以按需继续加载。</p>
    <div className="flex flex-wrap gap-2">{missing.map((section) => workspace.historyLoading.has(section)
      ? <Button key={section} type="button" variant="secondary" size="sm" onClick={() => workspace.cancelHistory(section)}>取消读取{historyLabels[section]}</Button>
      : <Button key={section} type="button" variant="secondary" size="sm" disabled={!workspace.connectionOnline} onClick={() => { setError(null); void workspace.loadHistory(section).catch(setError) }}>读取更多{historyLabels[section]}</Button>)}</div>
    <InlineError error={error} />
  </div>
}
