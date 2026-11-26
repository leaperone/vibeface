import { useEffect, useRef, useState } from 'react'
import type { useWorkspace } from '../workspace/context.tsx'
import type { InputTarget, NotificationContent } from '../workspace/types.ts'
import { messageOf } from '../lib/node.ts'

type Reader = Pick<ReturnType<typeof useWorkspace>, 'generation' | 'connectionOnline' | 'readNotificationRecord' | 'isCurrent' | 'questions'>

/** A precise notice read never enters Main's history and cannot outlive its route or connection. */
export function useNotificationRecord({ workspace, notificationId, conversationId, target }: { workspace: Reader | null; notificationId: string | null; conversationId: string; target: InputTarget }) {
  const key = JSON.stringify([workspace?.generation, conversationId, target, notificationId])
  const questions = JSON.stringify(workspace?.questions.map((question) => question.question_id) ?? [])
  const latest = useRef(key)
  latest.current = key
  const [result, setResult] = useState<{ key: string; content: NotificationContent } | { key: string; error: string } | null>(null)
  const [retry, setRetry] = useState(0)
  useEffect(() => {
    if (!workspace?.connectionOnline || !notificationId) return
    let active = true
    void workspace.readNotificationRecord({ notification_id: notificationId, conversation_id: conversationId, target }).then(
      (content) => { if (active && latest.current === key && workspace.isCurrent()) setResult({ key, content }) },
      (cause) => { if (active && latest.current === key && workspace.isCurrent()) setResult({ key, error: messageOf(cause) }) },
    )
    return () => { active = false }
  }, [key, workspace?.connectionOnline, questions, retry])
  if (!notificationId) return { kind: 'unavailable' as const, text: '这个例行记录没有原提醒标识，请从原通知重新打开。' }
  if (!workspace?.connectionOnline) return { kind: 'unavailable' as const, text: '原例行记录需要连接后读取；当前无法确认它的状态。' }
  if (result?.key === key) {
    if ('content' in result) return { kind: 'record' as const, content: result.content }
    return { kind: 'failed' as const, text: result.error, retry: () => { if (latest.current !== key) return; setResult(null); setRetry((value) => value + 1) } }
  }
  return { kind: 'loading' as const, text: '正在读取原例行记录…' }
}
