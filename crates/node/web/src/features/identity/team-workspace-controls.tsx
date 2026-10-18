import { useRef, useState } from 'react'
import { ChevronDown } from 'lucide-react'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { messageOf } from '@/lib/node'
import { useDevice } from './context'
import { TeamControls } from './team-controls'

export function TeamWorkspaceControls() {
  const device = useDevice()
  const pending = useRef(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const review = device.teamCreationReview
  const ready = device.ready
  if (!ready) return null
  async function run(action: () => Promise<void>) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError('')
    try { await action() } catch (error) { setError(messageOf(error)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <div className="space-y-5">
    {review && <Card className="gap-4 p-5" aria-labelledby="team-review-title">
      <h2 id="team-review-title" className="font-semibold">{review.committed ? '保存已确认的团队恢复包' : '保存恢复包并批准创建团队'}</h2>
      <dl className="grid gap-3 text-xs"><div><dt>所有者账户</dt><dd className="break-all font-mono">{review.accountId}</dd></div><div><dt>团队空间</dt><dd className="break-all font-mono">{review.teamId}</dd></div><div><dt>来源项目</dt><dd className="break-all font-mono">{review.sourceProjectId}</dd></div><div><dt>目标 Node 完整公钥</dt><dd className="break-all font-mono">{review.nodePublicKey}</dd></div></dl>
      <p className="text-sm text-muted-foreground">团队根只保存在加密恢复包中。团队沿用账户的恢复公钥，请同时保留原完整账户恢复包及口令。</p>
      {review.submitted && !review.committed && <p role="status" className="text-sm">原创建可能已生效。继续时只核对同一份请求，不创建另一个团队。</p>}
      <form key={`${review.teamId}/${review.committed}`} className="space-y-3" onSubmit={event => {
        event.preventDefault()
        const saved = event.currentTarget.elements.namedItem('saved')
        void run(() => device.confirmTeamCreation(saved instanceof HTMLInputElement && saved.checked))
      }}>
        <Button type="button" variant="outline" disabled={busy} onClick={() => void run(async () => device.downloadBackup())}>{review.committed ? '下载更新的团队恢复包' : '下载待提交的团队恢复包'}</Button>
        <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="saved" required disabled={busy} />我已保存当前恢复包，并单独保管口令</label>
        <Button type="submit" disabled={busy}>{review.committed ? '确认保存并进入团队项目' : review.submitted ? '核对原创建结果' : '确认保存并创建团队'}</Button>
      </form>
      <Button type="button" variant="outline" onClick={device.cancelSensitive}>关闭并清除本页材料</Button>
      {review.submitted && <p className="text-xs text-muted-foreground">关闭不会撤回已创建的团队。请先保存当前恢复包，再关闭或切换设备配置。</p>}
    </Card>}
    {device.governancePending && <Card className="gap-4 p-5">
      <h2 className="font-semibold">{device.governancePending}待核对</h2>
      <p className="text-sm text-muted-foreground">保留原签名政策请求。先核对结果，确认后才提交其他政策变更。</p>
      <Button type="button" disabled={busy} onClick={() => void run(device.retryGovernance)}>核对原政策提交</Button>
      <Button type="button" variant="outline" onClick={device.cancelSensitive}>关闭原请求</Button>
      <p className="text-xs text-muted-foreground">关闭不会撤回已生效的政策。重新连接后先核对当前成员和授权。</p>
    </Card>}
    {busy && <p role="status" className="text-sm text-muted-foreground">正在核对原团队或政策请求…</p>}
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    <Card className="gap-0 py-0"><Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="h-auto min-h-14 w-full justify-between px-5 py-4 text-left [&[data-state=open]>svg]:rotate-180"><span className="font-semibold">继续待确认的团队创建</span><ChevronDown className="size-4 shrink-0 transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <p className="text-sm text-muted-foreground">页面关闭或刷新后，选择提交前保存的待提交团队恢复包。使用原设备配置，连接原 Node 并选择原个人项目。会恢复原团队和原签名请求，先核对已接受的结果。</p>
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault()
          const form = event.currentTarget
          if (!form.reportValidity()) return
          const data = new FormData(form), file = data.get('bundle')
          if (!(file instanceof File)) return
          const input = { file, password: String(data.get('password') ?? '') }
          void run(async () => { try { await device.restoreTeamCreation(input) } finally { input.password = ''; form.reset() } })
        }}><fieldset disabled={busy || !!review || !!device.governancePending || !!device.nodeEnrollmentReview || !!device.review || device.firstPasskeyPending} className="space-y-3">
          <label className="grid gap-1 text-sm">待提交团队恢复包<Input name="bundle" type="file" accept="application/json,.json" required /></label>
          <label className="grid gap-1 text-sm">团队恢复包口令<Input name="password" type="password" autoComplete="off" required /></label>
          <Button type="submit">解锁并恢复原团队请求</Button>
        </fieldset></form>
      </CollapsibleContent>
    </Collapsible></Card>
    <TeamControls
      scopeToken={`${ready.generation}:${ready.scope.device_id}:${ready.project.id}`}
      accountId={ready.scope.account_id}
      spaceId={ready.scope.space_id}
      projectId={ready.project.id}
      nodePublicKey={ready.audiencePublicKey}
      teamGenesis={ready.authorization.isTeam ? ready.authorization.spaceGenesis : null}
      serviceDevices={(device.directory?.devices ?? []).flatMap(item => item.status === 'active' && item.subject.kind === 'service' ? [{ id: item.subject.id, publicKey: item.public_key }] : [])}
      disabled={busy || !!review || !!device.governancePending || !!device.nodeEnrollmentReview || !!device.review || device.firstPasskeyPending || !!device.incomingEnrollment}
      createTeam={device.createTeam}
      changeMembership={device.changeMembership}
      grant={device.grantProject}
      revokeGrant={device.revokeProjectGrant}
      registerExecutionService={device.registerExecutionService}
      latestExecutionServiceResource={device.latestExecutionServiceResource}
      executionServiceOptions={device.executionServiceOptions}
      refreshExecutionServices={device.refreshExecutionServices}
      selectExecutionService={device.selectExecutionService}
      downloadExecutionServiceBackup={device.downloadExecutionServiceBackup}
      executionServiceBackupAvailable={device.executionServiceBackupAvailable}
      createRemoteExecutionShare={device.createRemoteExecutionShare}
      latestRemoteExecutionShare={device.latestRemoteExecutionShare}
      inspectRemoteExecutionRequest={device.inspectRemoteExecutionRequest}
    />
  </div>
}
