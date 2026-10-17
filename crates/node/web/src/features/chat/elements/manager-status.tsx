import { useState } from 'react'
import { Card, CardContent } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Button } from '@/components/ui/button'
import { Field, FieldLabel } from '@/components/ui/field'
import { Textarea } from '@/components/ui/textarea'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton, StatusBadge, SubmitButton } from '@/features/workspace/present'
import type { ManagerJob } from '@/features/workspace/types'

const managerStates: Record<string, string> = {
  delivery_pending: '等待投递到调度端', queued: '等待处理', claimed: '正在准备回复', running: '正在回复',
  cancelling: '正在核对停止结果', cancelled: '本轮回复已停止', completed: '本轮回复已结束',
  failed: '本轮回复失败', rejected: '本轮请求被拒绝', unknown: '本轮结果待核对',
}

export function ManagerStatus({ job, progress, hasLiveProgress }: { job: ManagerJob; progress?: string; hasLiveProgress: boolean }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const canStop = workspace.data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state)
  return <Card className="gap-0 py-0">
    <CardContent className="space-y-3 p-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="mr-auto text-sm font-medium">{job.kind === 'message' || job.message_id ? '你的消息' : '任务后续汇报'}</span>
        <StatusBadge value={job.state} label={managerStates[job.state] ?? `状态：${job.state}`} />
        {job.read_only ? <span className="text-xs text-muted-foreground">只读交谈</span> : null}
      </div>
      {job.error ? <p className="whitespace-pre-wrap break-words text-sm">{job.error}</p> : null}
      {progress ? <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">{progress}</p> : null}
      {job.state === 'running' && !hasLiveProgress ? <p className="text-sm text-muted-foreground">当前只取得持久状态，尚未取得这轮实时片段；最终结果会在此显示。</p> : null}
      {canStop ? <>
        <ActionButton disabled={!workspace.writable} action={() => workspace.run(() => workspace.cancelManager(job.trigger))}>停止本轮回复</ActionButton>
        <p className="text-xs text-muted-foreground">只停止这次回复及尚未派出的安排，已派出的后台任务会继续单独跟踪。</p>
      </> : null}
      {job.state === 'unknown' ? <>
        <p className="text-sm">可继续只读聊天。原调用与外部结果仍需核对，不能直接重派。</p>
        <Collapsible>
          <CollapsibleTrigger asChild><Button type="button" variant="secondary">本人核对原调用</Button></CollapsibleTrigger>
          <CollapsibleContent className="pt-3">
            <p className="mb-3 text-sm text-muted-foreground">先在执行详情核对关联操作、原进程和外部结果。只有服务器确认旧工具权限已隔离、在途操作已有记录，才会解除写入冻结。</p>
            <form className="space-y-3" onSubmit={(event) => {
              event.preventDefault()
              const form = event.currentTarget
              if (!form.reportValidity()) return
              setBusy(true)
              void workspace.run(() => workspace.resolveManagerUnknown(job.trigger, String(new FormData(form).get('evidence') ?? ''))).finally(() => setBusy(false))
            }}>
              <Field><FieldLabel htmlFor={`manager-evidence-${job.trigger}`}>本人核查证据</FieldLabel><Textarea id={`manager-evidence-${job.trigger}`} name="evidence" rows={3} required disabled={!workspace.writable} /></Field>
              <SubmitButton busy={busy} disabled={!workspace.writable}>提交核查证据</SubmitButton>
            </form>
          </CollapsibleContent>
        </Collapsible>
        <Button type="button" variant="ghost" onClick={() => workspace.setView('sessions')}>查看执行详情与原操作</Button>
      </> : null}
    </CardContent>
  </Card>
}
