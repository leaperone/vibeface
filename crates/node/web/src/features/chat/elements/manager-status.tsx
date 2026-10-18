import { useState } from 'react'
import { CodeBlock, CodeBlockContent } from '@/components/ui/code-block'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Button } from '@/components/ui/button'
import { Field, FieldLabel } from '@/components/ui/field'
import { Textarea } from '@/components/ui/textarea'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton, StatusBadge, SubmitButton } from '@/features/workspace/present'
import type { ManagerJob } from '@/features/workspace/types'

const managerStates: Record<string, string> = {
  pending_delivery: '等待助手接收', queued: '等待处理', claimed: '正在准备回复', running: '正在回复',
  cancelling: '正在核对停止结果', cancelled: '本轮回复已停止', completed: '本轮回复已结束',
  failed: '本轮回复失败', rejected: '本轮请求被拒绝', unknown: '本轮结果待核对',
}

export function managerTraceTitle(job: ManagerJob) {
  const label = job.kind === 'message' || job.message_id ? '你的消息' : '后台进展'
  return `${label} · ${managerStates[job.state] ?? job.state}`
}

export function managerTraceStatus(state: string): 'running' | 'error' | 'pending' | undefined {
  if (['failed', 'rejected'].includes(state)) return 'error'
  if (state === 'unknown' || state === 'pending_delivery') return 'pending'
  if (['queued', 'claimed', 'running', 'cancelling'].includes(state)) return 'running'
  return undefined
}

export function ManagerStatus({ job, progress, hasLiveProgress }: { job: ManagerJob; progress?: string; hasLiveProgress: boolean }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const canStop = workspace.data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state)
  return <div className="grid min-w-0 gap-3">
    <div className="flex flex-wrap items-center gap-2">
      <StatusBadge value={job.state} label={managerStates[job.state] ?? `状态：${job.state}`} />
      {job.read_only ? <span className="text-xs text-muted-foreground">只读交谈</span> : null}
    </div>
    {job.error ? <CodeBlock size="sm" variant="destructive"><CodeBlockContent wrap className="max-h-40">{job.error}</CodeBlockContent></CodeBlock> : null}
    {job.delivery_error ? <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">投递提示：{job.delivery_error}</p> : null}
    {progress ? <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">{progress}</p> : null}
    {job.state === 'running' && !hasLiveProgress ? <p className="text-sm text-muted-foreground">还没有实时片段。结果会写在对话里。</p> : null}
    {canStop ? <>
      <ActionButton disabled={!workspace.writable || !workspace.selectedBot} action={() => workspace.run(() => workspace.cancelManager(job.trigger))}>停止本轮回复</ActionButton>
      <p className="text-xs text-muted-foreground">只停这次回复。</p>
    </> : null}
    {job.state === 'unknown' ? <>
      <p className="text-sm">可继续只读聊天。原调用仍需核对，不能直接重派。</p>
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="secondary" className="min-h-11">本人核对原调用</Button></CollapsibleTrigger>
        <CollapsibleContent className="pt-3">
          <p className="mb-3 text-sm text-muted-foreground">先核对关联操作和外部结果。服务器确认旧权限已隔离后，才会解除写入冻结。</p>
          <form className="space-y-3" onSubmit={(event) => {
            event.preventDefault()
            const form = event.currentTarget
            if (!form.reportValidity()) return
            setBusy(true)
            void workspace.run(() => workspace.resolveManagerUnknown(job.trigger, String(new FormData(form).get('evidence') ?? ''))).finally(() => setBusy(false))
          }}>
            <Field><FieldLabel htmlFor={`manager-evidence-${job.trigger}`}>本人核查证据</FieldLabel><Textarea id={`manager-evidence-${job.trigger}`} name="evidence" rows={3} required disabled={!workspace.writable || !workspace.selectedBot} /></Field>
            <SubmitButton busy={busy} disabled={!workspace.writable || !workspace.selectedBot}>提交核查证据</SubmitButton>
          </form>
        </CollapsibleContent>
      </Collapsible>
    </> : null}
  </div>
}
