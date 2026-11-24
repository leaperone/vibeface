import { useEffect, useRef, useState } from 'react'
import type { useWorkspace } from '../workspace/context.tsx'
import { messageOf } from '../lib/node.ts'

type Reader = Pick<ReturnType<typeof useWorkspace>, 'generation' | 'conversationId' | 'data' | 'dataOrigin' | 'connectionOnline' | 'historyLoading' | 'loadHistory' | 'isCurrent'>

/** A focused record advances each cursor once. A failed or cancelled read needs an explicit retry. */
export function useHistoryLocation({ workspace, active, focus, found }: { workspace: Reader | null; active: boolean; focus: string | null; found: boolean }) {
  const section = focus?.startsWith('question:') ? 'questions' : 'messages'
  const coverage = workspace?.data?.history[section]
  const visit = JSON.stringify([workspace?.generation, workspace?.conversationId, focus])
  const cursor = JSON.stringify([visit, workspace?.data?.history_authority, section, coverage?.next_cursor])
  const latest = useRef({ visit, cursor, active })
  latest.current = { visit, cursor, active }
  const attempted = useRef({ visit, cursors: new Set<string>(), failed: false })
  if (attempted.current.visit !== visit) attempted.current = { visit, cursors: new Set(), failed: false }
  const [failure, setFailure] = useState<{ visit: string; text: string } | null>(null)
  const [retry, setRetry] = useState(0)
  useEffect(() => {
    if (!active || !focus || found || !workspace?.isCurrent() || !workspace.connectionOnline || workspace.dataOrigin !== 'network' || !coverage || coverage.complete || workspace.historyLoading.has(section) || attempted.current.failed || attempted.current.cursors.has(cursor)) return
    attempted.current.cursors.add(cursor)
    void workspace.loadHistory(section).catch((cause) => {
      if (workspace.isCurrent() && latest.current.active && latest.current.visit === visit) {
        attempted.current.failed = true
        setFailure({ visit, text: messageOf(cause) })
      }
    })
  }, [active, focus, found, visit, cursor, coverage?.complete, workspace?.connectionOnline, workspace?.dataOrigin, workspace?.historyLoading, retry])
  if (!active || !focus || found) return { kind: 'found' as const }
  const noun = section === 'questions' ? '原问题' : '原消息'
  if (!workspace?.connectionOnline || workspace.dataOrigin !== 'network') return { kind: 'waiting' as const, text: `正在显示最近记录；连接后再定位${noun}。` }
  if (coverage?.complete) return { kind: 'unavailable' as const, text: `${noun}已不在这段对话记录中。请查看最新结果，旧提醒不会再次执行。` }
  if (failure?.visit === visit || attempted.current.cursors.has(cursor) && !workspace.historyLoading.has(section)) return {
    kind: 'failed' as const,
    text: failure?.visit === visit ? `未能定位${noun}：${failure.text}` : `${noun}定位已暂停。`,
    retry: () => { if (latest.current.cursor !== cursor || !latest.current.active) return; attempted.current.failed = false; attempted.current.cursors.delete(cursor); setFailure(null); setRetry((value) => value + 1) },
  }
  return { kind: 'loading' as const, text: `正在读取更早记录以定位${noun}。` }
}
