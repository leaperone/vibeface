import { useEffect, useRef, useState } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { NativeSelect } from '@/components/ui/native-select'
import { Textarea } from '@/components/ui/textarea'
import { messageOf, record } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { parseRemoteExecutionShare, parseRemoteExecutionShareInput, parseRemoteExecutionSourceTrust } from '@/lib/remote-execution'
import type { RemoteExecutionRequestSummary, RemoteExecutionShareAction, RemoteExecutionShareInput } from '@/lib/remote-execution'

export type TeamRole = 'viewer' | 'operator' | 'resource_admin' | 'space_admin'
export type TeamGrantee =
  | { kind: 'actor'; value: { kind: 'account' | 'service'; id: string } }
  | { kind: 'members_of'; value: { space: string; pinned_genesis: string; kind: 'account'; roles_any: TeamRole[] } }

const roleChoices: { value: TeamRole; label: string }[] = [
  { value: 'viewer', label: '查看者' },
  { value: 'operator', label: '操作者' },
  { value: 'resource_admin', label: '资源管理员' },
  { value: 'space_admin', label: '空间管理员' },
]
const actionLabels = {
  conversation_read: '读取会话', conversation_create: '创建会话', message_append: '添加消息',
  task_create: '创建任务', task_progress: '更新任务', task_accept: '接受任务', task_cancel: '取消任务',
  execution_open: '打开执行会话', execution_send: '发送执行指令', execution_steer: '补充执行指令',
  execution_interrupt: '中断执行', execution_respond_approval: '回应执行审批', execution_rebind: '重新绑定执行',
  operation_status_read: '读取操作结果', operation_reconcile: '核对未知结果', operation_resolve: '裁定未知结果',
  attachment_read: '读取附件', attachment_write: '写入附件', export: '导出项目',
  notification_read: '读取通知', notification_ack: '确认通知', member_manage: '管理成员',
  resource_manage: '管理资源', grant_issue: '签发授权', grant_revoke: '撤销授权',
  service_enroll: '登记服务', replica_read: '读取同步副本', agent_start: '启动 Agent',
}
export type TeamGrantAction = keyof typeof actionLabels
const actionChoices = Object.entries(actionLabels).map(([value, label]) => ({ value, label }))

export type TeamGrantInput = {
  grantee: TeamGrantee
  scope: { kind: 'exact'; value: { space: string; resource: string } }
  actions: TeamGrantAction[]
  audienceNodePublicKey: string
  constraints: {
    context: { conversation: null; task: null; sdk_session: null; execution_session: null; executor_kind: null; workspace_resource: null }
    require_account_actor: boolean
    require_webauthn_uv: boolean
  }
  remainingDelegations: number
  expiresAt: number
}
export type TeamControlsProps = {
  scopeToken: string
  accountId: string
  spaceId: string
  projectId: string
  nodePublicKey: string
  teamGenesis: string | null
  serviceDevices: { id: string; publicKey: string }[]
  disabled: boolean
  createTeam(input: { password: string; certificateExpiresAt: number }): Promise<void>
  changeMembership(input: { subjectAccount: string; roles: TeamRole[]; expiresAt: number }): Promise<void>
  grant(input: TeamGrantInput): Promise<void>
  revokeGrant(grantHash: string): Promise<void>
  createRemoteExecutionShare(input: RemoteExecutionShareInput): Promise<string>
  latestRemoteExecutionShare?: string
  inspectRemoteExecutionRequest(input: { requestJson: string; sourceTrustJson?: string; confirmedSourceRoots: boolean }): Promise<RemoteExecutionRequestSummary>
  registerExecutionService(input: { file: File; password: string; expiresAt: number }): Promise<string>
  latestExecutionServiceResource?: string
  executionServiceOptions: { spaceId: string; resourceId: string }[]
  refreshExecutionServices(): Promise<void>
  selectExecutionService(resourceId: string): Promise<void>
  downloadExecutionServiceBackup(): void
  executionServiceBackupAvailable: boolean
}

const disclosure = 'h-auto min-h-14 w-full justify-between gap-3 rounded-none px-5 py-4 text-left whitespace-normal [&[data-state=open]>svg]:rotate-180'
const input = 'min-h-10'
const button = 'min-h-10'
const accountPattern = 'acc2_[a-f0-9]{64}'
const hashPattern = 'h2_[a-f0-9]{64}'
const remoteShareChoices: { value: RemoteExecutionShareAction; label: string }[] = [
  { value: 'execution_open', label: '打开此执行会话' },
  { value: 'operation_status_read', label: '读取此会话的执行状态' },
  { value: 'execution_send', label: '发送新一轮任务' },
  { value: 'execution_steer', label: '补充当前轮指令' },
  { value: 'execution_interrupt', label: '中断当前轮' },
  { value: 'execution_respond_approval', label: '回应执行审批' },
]

function remoteShareInput(data: FormData, requestJson: string, executionResourceId: string): RemoteExecutionShareInput {
  if (!data.has('service_confirmed')) throw new Error('请单独确认已核验的 B 项目及本次执行范围。')
  const purpose = data.get('remote_purpose')
  if (purpose !== 'execution' && purpose !== 'resolution' && purpose !== 'rebind') throw new Error('请选择本次明确批准的授权用途。')
  const actions = purpose === 'rebind' ? ['execution_rebind', 'operation_status_read'] : purpose === 'resolution' ? ['execution_resolve', 'operation_resolve'] : ['execution_open', 'operation_status_read', ...data.getAll('remote_actions')]
  return parseRemoteExecutionShareInput({ requestJson, executionResourceId, actions, expiresAt: expiry(data) }, Date.now())
}

function downloadRemoteShare(shareJson: string) {
  const share = parseRemoteExecutionShare(JSON.parse(shareJson))
  const url = URL.createObjectURL(new Blob([shareJson], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url; link.download = `vibeface-execution-${share.selection.execution_grant}.json`
  link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000)
}

function scopedRemoteShare(shareJson: string, resourceId: string, nodePublicKey: string) {
  if (!shareJson) return ''
  try {
    const share = parseRemoteExecutionShare(JSON.parse(shareJson))
    return share.selection.execution_resource.resource === resourceId && share.selection.target_public_key === nodePublicKey ? shareJson : ''
  } catch { return '' }
}

function field(data: FormData, name: string) {
  const value = data.get(name)
  return typeof value === 'string' ? value : ''
}
function expiry(data: FormData) {
  const value = Date.parse(field(data, 'expires_at'))
  if (!Number.isSafeInteger(value) || value <= Date.now()) throw new Error('请选择晚于当前时刻的有效期。')
  return value
}
function selectedRoles(data: FormData, name: string) {
  const values = data.getAll(name)
  if (values.some(value => !roleChoices.some(choice => choice.value === value))) throw new Error('包含不支持的成员角色。')
  return roleChoices.filter(choice => values.includes(choice.value)).map(choice => choice.value)
}
function isAction(value: FormDataEntryValue): value is TeamGrantAction {
  return typeof value === 'string' && Object.hasOwn(actionLabels, value)
}
function membershipInput(data: FormData, revoke: boolean): Parameters<TeamControlsProps['changeMembership']>[0] {
  const subjectAccount = field(data, 'subject_account').trim()
  if (!new RegExp(`^${accountPattern}$`).test(subjectAccount)) throw new Error('请填写完整的成员账户 ID。')
  const roles = revoke ? [] : selectedRoles(data, 'roles')
  if (!revoke && !roles.length) throw new Error('请至少选择一个成员角色。')
  if (revoke && !data.has('revoke_confirmed')) throw new Error('请单独确认撤销该成员。')
  return { subjectAccount, roles, expiresAt: expiry(data) }
}
function grantInput(data: FormData, props: TeamControlsProps): TeamGrantInput {
  let grantee: TeamGrantee
  switch (field(data, 'grantee_kind')) {
    case 'actor': {
      const id = field(data, 'grantee_account').trim()
      if (!new RegExp(`^${accountPattern}$`).test(id)) throw new Error('请填写完整的受让账户 ID。')
      grantee = { kind: 'actor', value: { kind: 'account', id } }
      break
    }
    case 'service': {
      const id = field(data, 'grantee_service').trim()
      if (!props.serviceDevices.some(device => device.id === id)) throw new Error('请选择已核验的账户 Node 服务设备。')
      grantee = { kind: 'actor', value: { kind: 'service', id } }
      break
    }
    case 'members_of': {
      if (!props.teamGenesis || !new RegExp(`^${hashPattern}$`).test(props.teamGenesis)) throw new Error('当前空间没有已固定的团队 genesis。')
      if (field(data, 'trusted_genesis').trim() !== props.teamGenesis || !data.has('genesis_confirmed')) throw new Error('请从可信渠道核对完整团队 genesis。')
      const roles = selectedRoles(data, 'selector_roles')
      if (!roles.length) throw new Error('请至少选择一个团队成员角色。')
      grantee = { kind: 'members_of', value: { space: props.spaceId, pinned_genesis: props.teamGenesis, kind: 'account', roles_any: roles } }
      break
    }
    default: throw new Error('请选择受让主体。')
  }
  const values = data.getAll('actions')
  if (!values.length || !values.every(isAction)) throw new Error('请明确选择至少一项允许的操作。')
  const actions = actionChoices.map(choice => choice.value).filter(isAction).filter(value => values.includes(value))
  const audienceNodePublicKey = field(data, 'audience_node').trim()
  if (!/^[a-f0-9]{64}$/.test(audienceNodePublicKey)) throw new Error('请填写完整的 64 位 Node 公钥。')
  if (grantee.kind === 'actor' && grantee.value.kind === 'service' && !props.serviceDevices.some(device => device.id === grantee.value.id && device.publicKey === audienceNodePublicKey)) throw new Error('服务授权的执行 Node 公钥必须与所选已核验服务设备相同。')
  const delegation = field(data, 'delegations').trim()
  if (!/^[0-8]$/.test(delegation)) throw new Error('可继续转授的层数需要在 0 至 8 之间。')
  const remainingDelegations = Number(delegation)
  return {
    grantee, scope: { kind: 'exact', value: { space: props.spaceId, resource: props.projectId } },
    actions,
    audienceNodePublicKey,
    constraints: { context: { conversation: null, task: null, sdk_session: null, execution_session: null, executor_kind: null, workspace_resource: null }, require_account_actor: grantee.kind !== 'actor' || grantee.value.kind === 'account', require_webauthn_uv: data.has('require_uv') },
    remainingDelegations, expiresAt: expiry(data),
  }
}

export function TeamControls(props: TeamControlsProps) {
  return <TeamForms key={`${props.scopeToken}/${props.accountId}/${props.spaceId}/${props.projectId}/${props.nodePublicKey}/${props.teamGenesis}`} {...props} />
}

function TeamForms(props: TeamControlsProps) {
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const activeScope = useRef<string | null>(props.scopeToken)
  useEffect(() => { activeScope.current = props.scopeToken; return () => { activeScope.current = null } }, [props.scopeToken])
  const [feedback, setFeedback] = useState<{ kind: 'error' | 'status'; message: string } | null>(null)
  const [revokeMember, setRevokeMember] = useState(false)
  const [granteeKind, setGranteeKind] = useState('actor')
  const [generatedRemoteShare, setGeneratedRemoteShare] = useState('')
  const executionResourceId = props.latestExecutionServiceResource && /^res2_[a-f0-9]{64}$/.test(props.latestExecutionServiceResource) ? props.latestExecutionServiceResource : ''
  const [executionRequest, setExecutionRequest] = useState<{ json: string; summary: RemoteExecutionRequestSummary } | null>(null)
  const [sourceTrustJson, setSourceTrustJson] = useState('')
  const [remotePurpose, setRemotePurpose] = useState('execution')
  useEffect(() => {
    setGeneratedRemoteShare('')
  }, [executionResourceId])
  const remoteShare = scopedRemoteShare(props.latestRemoteExecutionShare || generatedRemoteShare, executionResourceId, props.nodePublicKey)
  const membersOf = granteeKind === 'members_of'
  const disabled = props.disabled || busy

  async function run(action: (requireCurrent: () => void) => Promise<void>, success: string) {
    if (pending.current || props.disabled) return
    const token = activeScope.current
    const requireCurrent = () => { if (token === null || activeScope.current !== token) throw new Error('当前设备配置或项目已切换，请在当前入口重新核验。') }
    pending.current = true; setBusy(true); setFeedback(null)
    try { await action(requireCurrent); requireCurrent(); setFeedback({ kind: 'status', message: success }) }
    catch (cause) { setFeedback({ kind: 'error', message: messageOf(cause) }) }
    finally { pending.current = false; setBusy(false) }
  }
  async function copyRemoteShare() {
    try {
      if (!navigator.clipboard) throw new Error('当前浏览器无法直接复制，请下载分享文件。')
      await navigator.clipboard.writeText(remoteShare)
      setFeedback({ kind: 'status', message: '执行分享已复制。' })
    } catch (cause) { setFeedback({ kind: 'error', message: messageOf(cause) }) }
  }

  return <div className="space-y-5">
    {busy && <p role="status" className="text-sm text-muted-foreground">正在等待 Node 确认。断线或超时后，请先核对原请求结果。</p>}
    {feedback && <p role={feedback.kind === 'error' ? 'alert' : 'status'} className={feedback.kind === 'error' ? 'text-sm text-destructive' : 'text-sm text-muted-foreground'}>{feedback.message}</p>}

    {!props.teamGenesis && <Card className="gap-0 py-0"><Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">将当前项目共享到新团队</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <p className="text-sm text-muted-foreground">创建独立团队空间，让团队项目使用当前项目的同一工程目录。原个人项目和原授权继续保留。成员使用各自账户，团队恢复包需要单独下载并确认保存。</p>
        <p className="break-all font-mono text-xs text-muted-foreground">{props.projectId}</p>
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const form = event.currentTarget
          void run(async () => {
            const data = new FormData(form)
            try {
              if (!data.has('transfer_confirmed')) throw new Error('请确认将当前项目的工程目录共享到新团队。')
              await props.createTeam({ password: passphrase(field(data, 'password'), field(data, 'confirmation')), certificateExpiresAt: expiry(data) })
            } finally {
              form.querySelectorAll<HTMLInputElement>('input[type="password"]').forEach(element => { element.value = '' })
              data.delete('password'); data.delete('confirmation')
            }
          }, '团队材料已准备。请先下载并确认保存恢复包，再批准提交。')
        }}><fieldset disabled={disabled} className="space-y-3">
          <label className="grid gap-1 text-sm">团队恢复包口令，至少 12 个字符<Input className={input} name="password" type="password" minLength={12} autoComplete="new-password" required /></label>
          <label className="grid gap-1 text-sm">再次填写口令<Input className={input} name="confirmation" type="password" minLength={12} autoComplete="new-password" required /></label>
          <label className="grid gap-1 text-sm">团队设备证书与初始授权有效期，本地时间<Input className={input} name="expires_at" type="datetime-local" required /></label>
          <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="transfer_confirmed" required />我确认将当前项目的工程目录共享到新团队，原访问继续保留</label>
          <Button className={button} type="submit">准备团队与恢复包</Button>
        </fieldset></form>
      </CollapsibleContent>
    </Collapsible></Card>}

    <Card className="gap-0 py-0"><Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">管理当前空间成员</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <p className="text-sm text-muted-foreground">只有具备对应管理权限的设备才能提交。成员关系与项目操作授权分别管理。</p>
        <p className="break-all font-mono text-xs text-muted-foreground">{props.spaceId}</p>
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const data = new FormData(event.currentTarget)
          void run(() => props.changeMembership(membershipInput(data, revokeMember)), revokeMember ? '成员撤销结果已核验。' : '成员角色变更已核验。')
        }}><fieldset disabled={disabled} className="space-y-3">
          <label className="grid gap-1 text-sm">成员账户 ID<Input className={`${input} font-mono text-xs`} name="subject_account" pattern={accountPattern} autoComplete="off" spellCheck={false} required /></label>
          <label className="grid gap-1 text-sm">成员变更<NativeSelect className="w-full" value={revokeMember ? 'revoke' : 'set'} onChange={event => setRevokeMember(event.target.value === 'revoke')}><option value="set">设置成员角色</option><option value="revoke">撤销成员</option></NativeSelect></label>
          {revokeMember ? <div role="alert" className="space-y-2 rounded-lg border border-destructive/40 p-3 text-sm">
            <p>撤销后，该账户将失去当前空间的直接成员关系，依赖此关系的授权也会受影响。</p>
            <label className="flex min-h-11 items-center gap-2"><input type="checkbox" name="revoke_confirmed" required />我确认撤销上述账户的成员关系</label>
          </div> : <fieldset className="space-y-2"><legend className="text-sm">成员角色，可多选</legend><div className="grid gap-1 sm:grid-cols-2">{roleChoices.map(choice => <label key={choice.value} className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="roles" value={choice.value} />{choice.label}</label>)}</div></fieldset>}
          <label className="grid gap-1 text-sm">本次成员记录有效期，本地时间<Input className={input} name="expires_at" type="datetime-local" required /></label>
          <Button className={button} type="submit" variant={revokeMember ? 'destructive' : 'default'}>{revokeMember ? '确认撤销成员' : '保存成员角色'}</Button>
        </fieldset></form>
      </CollapsibleContent>
    </Collapsible></Card>

    <Card className="gap-0 py-0"><Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">让另一个项目使用此 Node 受限执行</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <p className="text-sm text-muted-foreground">导入 B 项目提供的受签执行申请，核验后再授权。B 可以属于另一个账户。授权只覆盖这一任务和执行会话，禁止继续转授。</p>
        <p className="text-xs text-muted-foreground">当前项目 · {props.projectId.slice(5, 13)}，执行 Node · {props.nodePublicKey.slice(0, 8)}。</p>
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const form = event.currentTarget
          if (!form.reportValidity()) return
          const resourceId = field(new FormData(form), 'execution_resource')
          void run(async requireCurrent => {
            if (!props.executionServiceOptions.some(option => option.resourceId === resourceId)) throw new Error('请选择当前列表中已登记的执行资源。')
            setGeneratedRemoteShare('')
            await props.selectExecutionService(resourceId)
            requireCurrent()
          }, '已选择并核验原执行资源。')
        }}><fieldset disabled={disabled} className="space-y-3 rounded-lg border border-border p-3">
          <legend className="px-1 text-sm font-medium">使用已登记的执行资源</legend>
          <p className="text-xs text-muted-foreground">刷新或重新解锁后，可读取并选择原执行资源，继续处理原操作的未知结果。选择已有资源不需要恢复包。</p>
          <Button type="button" variant="outline" onClick={() => void run(props.refreshExecutionServices, '已读取并核验当前项目的执行资源。')}>读取已登记的执行资源</Button>
          <label className="grid gap-1 text-sm">已登记的执行资源<NativeSelect className="w-full" name="execution_resource" defaultValue="" required disabled={!props.executionServiceOptions.length}><option value="" disabled>请选择已登记的执行资源</option>{props.executionServiceOptions.map(option => <option key={option.resourceId} value={option.resourceId} title={option.resourceId}>已登记的执行资源 · {option.resourceId.slice(5, 13)} · {option.resourceId.slice(-8)}</option>)}</NativeSelect></label>
          <Button type="submit" variant="outline" disabled={!props.executionServiceOptions.length}>选择并核验执行资源</Button>
        </fieldset></form>
        {executionResourceId && <div role="status" className="space-y-1 text-sm"><p>当前已核验的执行资源 · {executionResourceId.slice(5, 13)}</p><p className="break-all font-mono text-xs text-muted-foreground">{executionResourceId}</p></div>}
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const form = event.currentTarget
          if (!form.reportValidity()) return
          const data = new FormData(form)
          void run(async requireCurrent => {
            if (!data.has('resource_confirmed')) throw new Error('请确认将当前项目的允许目录登记为受限执行资源。')
            const file = data.get('recovery')
            if (!(file instanceof File)) throw new Error('选择物理 Node 所属个人账户的完整恢复包。')
            try {
              const registration = { file, password: field(data, 'password'), expiresAt: expiry(data) }
              const resourceId = await props.registerExecutionService(registration)
              requireCurrent()
              if (!/^res2_[a-f0-9]{64}$/.test(resourceId)) throw new Error('Node 未返回已确认的执行资源。')
            } finally {
              data.delete('password')
              const password = form.elements.namedItem('password')
              if (password instanceof HTMLInputElement) password.value = ''
            }
          }, '当前项目的受限执行资源已登记并核验。')
        }}><fieldset disabled={disabled} className="space-y-3 rounded-lg border border-border p-3">
          <legend className="px-1 text-sm font-medium">登记当前项目的执行资源</legend>
          <p className="text-xs text-muted-foreground">沿用当前项目在此 Node 上已配置的允许目录。Node 会核验原项目和资源管理权限，不允许借此登记任意主机目录。</p>
          <p className="text-xs text-muted-foreground">执行资源登记在物理 Node 所属个人空间。原项目可以来自个人空间或团队。恢复包只在本地解锁，根仅批准这个新执行资源。</p>
          <label className="grid gap-1 text-sm">物理 Node 所属账户的完整恢复包<Input className={input} name="recovery" type="file" accept="application/json,.json" required /></label>
          <label className="grid gap-1 text-sm">恢复包口令<Input className={input} name="password" type="password" autoComplete="off" required /></label>
          <label className="grid gap-1 text-sm">本次登记请求有效期，本地时间<Input className={input} name="expires_at" type="datetime-local" required /></label>
          <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="resource_confirmed" required />我确认使用当前项目的允许目录作为受限执行资源</label>
          <Button type="submit" variant="outline">为当前项目登记执行资源</Button>
        </fieldset></form>
        {props.executionServiceBackupAvailable && <div className="space-y-3 rounded-lg border border-border p-3"><p className="text-sm">执行资源登记后，账户恢复包已有更新。下载并单独保存新包，下次登记或恢复时使用新包。</p><Button type="button" variant="outline" onClick={props.downloadExecutionServiceBackup}>下载更新的账户恢复包</Button></div>}
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const form = event.currentTarget
          if (!form.reportValidity()) return
          const data = new FormData(form), file = data.get('request')
          void run(async requireCurrent => {
            setExecutionRequest(null)
            if (!(file instanceof File) || file.size > 65_536) throw new Error('选择 B 项目导出的完整受签申请文件，且不超过 64 KiB。')
            const json = await file.text()
            requireCurrent()
            const summary = await props.inspectRemoteExecutionRequest({ requestJson: json, ...(sourceTrustJson ? { sourceTrustJson } : {}), confirmedSourceRoots: data.has('source_roots_confirmed') })
            requireCurrent()
            setExecutionRequest({ json, summary })
          }, 'B 的受签项目、任务及执行申请已核验。')
        }}><fieldset disabled={disabled} className="space-y-3">
          <label className="grid gap-1 text-sm">B 项目导出的受签执行申请<Input className={input} name="request" type="file" accept="application/json,.json" required onChange={() => setExecutionRequest(null)} /></label>
          <p className="text-xs text-muted-foreground">优先使用已独立保存的 B 账户根信任。首次联系时，再从另一个可信渠道取得 B 信任卡，不能从申请或未知 Node 响应中自动学习根。</p>
          <label className="grid gap-1 text-sm">首次联系的独立 B 信任卡，可选<Input className={input} type="file" accept="application/json,.json" onChange={event => {
            const file = event.currentTarget.files?.[0]
            setSourceTrustJson(''); setExecutionRequest(null)
            if (!file) return
            void run(async requireCurrent => {
              if (file.size > 65_536) throw new Error('信任卡超过大小限制。')
              const json = await file.text()
              requireCurrent()
              const card = record(JSON.parse(json))
              if (card.version !== 2 || card.kind !== 'vibeface-source-trust') throw new Error('选择 B 项目独立导出的完整来源信任卡。')
              const source = parseRemoteExecutionSourceTrust(card)
              setSourceTrustJson(JSON.stringify({ version: 2, kind: 'vibeface-source-trust', ...source }))
            }, '已读取独立 B 信任卡。确认完整根公钥后再核验申请。')
          }} /></label>
          {sourceTrustJson && <div className="space-y-3 rounded-lg border border-border p-3"><p className="text-sm">待独立核对的账户、空间根及 B Node 公钥</p><Textarea value={JSON.stringify(JSON.parse(sourceTrustJson), null, 2)} readOnly rows={7} className="font-mono text-xs" aria-label="独立 B 信任卡的完整根与 Node 公钥" /><label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="source_roots_confirmed" required />我已通过独立可信渠道核对上方完整根与 B Node 公钥，同意保存此来源信任</label></div>}
          <Button type="submit" variant="outline">导入并核验执行申请</Button>
        </fieldset></form>
        {executionRequest && <div className="space-y-3 rounded-lg border border-border p-3">
          <dl className="space-y-2 text-sm"><div><dt className="text-muted-foreground">来源项目</dt><dd>{executionRequest.summary.projectLabel || 'B 项目'} · {executionRequest.summary.projectResourceId.slice(5, 13)}</dd></div><div><dt className="text-muted-foreground">任务</dt><dd>{executionRequest.summary.taskId}</dd></div><div><dt className="text-muted-foreground">执行器</dt><dd>{executionRequest.summary.executorKind}</dd></div><div><dt className="text-muted-foreground">来源 Service</dt><dd>已核验 · {executionRequest.summary.servicePublicKey.slice(0, 8)}</dd></div></dl>
          <Collapsible><CollapsibleTrigger asChild><Button type="button" variant="ghost" className="gap-2 px-0 text-sm">查看完整受签范围<ChevronDown className="size-4" /></Button></CollapsibleTrigger><CollapsibleContent className="pt-3"><Textarea value={executionRequest.json} readOnly rows={5} className="font-mono text-xs" aria-label="已核验的完整 B 执行申请" /></CollapsibleContent></Collapsible>
        </div>}
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const form = event.currentTarget
          if (!form.reportValidity()) return
          const data = new FormData(form)
          void run(async requireCurrent => {
            if (!executionRequest) throw new Error('先导入并核验 B 项目的受签申请。')
            if (!executionResourceId) throw new Error('先选择并核验当前项目的执行资源。')
            const result = await props.createRemoteExecutionShare(remoteShareInput(data, executionRequest.json, executionResourceId))
            requireCurrent()
            parseRemoteExecutionShare(JSON.parse(result))
            setGeneratedRemoteShare(result)
          }, 'C 的有限授权已核验。可以下载分享，再通过另一个可信渠道交付 C 根公钥。')
        }}><fieldset disabled={disabled || !executionRequest || !executionResourceId} className="space-y-3">
          <label className="grid gap-1 text-sm">本次授权用途<NativeSelect className="w-full" name="remote_purpose" value={remotePurpose} onChange={event => setRemotePurpose(event.target.value)}><option value="execution">打开和操作执行会话</option><option value="resolution">处理原操作的未知结果</option><option value="rebind">Rebind 授权，封存原执行会话</option></NativeSelect></label>
          <p className="text-sm">授权覆盖这个执行资源沿用的项目允许目录范围。B 会另外签名绑定实际工作目录，C 执行时会独立检查该目录是否在允许范围内。</p>
          {remotePurpose === 'execution' ? <fieldset className="space-y-2"><legend className="text-sm">这一执行会话允许的操作</legend><div className="grid gap-1 sm:grid-cols-2">{remoteShareChoices.map(choice => {
            const required = choice.value === 'execution_open' || choice.value === 'operation_status_read'
            return <label key={choice.value} className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="remote_actions" value={choice.value} checked={required ? true : undefined} disabled={required} readOnly={required} />{choice.label}{required ? '，此入口必需' : ''}</label>
          })}</div></fieldset> : remotePurpose === 'rebind' ? <p className="text-sm">此授权只允许封存所列原执行会话并查询封存结果，不能打开、发送指令或处理未知执行结果。封存确认后，B 的重绑与新 C 的执行仍需各自授权。</p> : <p className="text-sm">此分享只允许核验原执行结果并明确裁定未知状态。每次处理仍需本人核查、双方当前授权和执行端签名证据。</p>}
          <label className="grid gap-1 text-sm">授权失效时间，本地时间<Input className={input} name="expires_at" type="datetime-local" required /></label>
          <label className="flex min-h-11 items-start gap-2 text-sm"><input className="mt-1" type="checkbox" name="service_confirmed" required />我已确认上述 B 项目与任务，同意在已登记的项目允许目录范围内授予所选权限，并禁止继续转授</label>
          <Button className={button} type="submit">{remotePurpose === 'execution' ? '确认范围并生成执行分享' : remotePurpose === 'rebind' ? '确认范围并生成 Rebind 授权' : '确认范围并生成结果处理分享'}</Button>
        </fieldset></form>
      </CollapsibleContent>
    </Collapsible></Card>
    {remoteShare && <Card className="space-y-3 p-5">
      <h3 className="font-semibold">已核验的远端执行分享</h3>
      <p className="text-sm text-muted-foreground">将此分享交给 B 的操作者。C 的账户和空间根公钥需要通过另一个可信渠道独立核对，分享本身不建立根信任。</p>
      <Textarea value={remoteShare} readOnly rows={5} className="font-mono text-xs" aria-label="已核验的远端执行授权分享" />
      <div className="flex flex-wrap gap-2"><Button type="button" variant="outline" onClick={() => void copyRemoteShare()}>复制分享</Button><Button type="button" variant="outline" onClick={() => { try { downloadRemoteShare(remoteShare); setFeedback({ kind: 'status', message: '执行分享已下载。' }) } catch (cause) { setFeedback({ kind: 'error', message: messageOf(cause) }) } }}>下载分享</Button></div>
    </Card>}

    <Card className="gap-0 py-0"><Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">授权当前项目</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <p className="text-sm text-muted-foreground">授权仅覆盖以下项目。请明确选择受让主体、可执行的操作及执行 Node。</p>
        <dl className="space-y-2 text-xs"><div><dt className="text-muted-foreground">空间</dt><dd className="break-all font-mono">{props.spaceId}</dd></div><div><dt className="text-muted-foreground">项目</dt><dd className="break-all font-mono">{props.projectId}</dd></div></dl>
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const data = new FormData(event.currentTarget)
          void run(() => props.grant(grantInput(data, props)), '项目授权提交结果已核验。')
        }}><fieldset disabled={disabled} className="space-y-3">
          <label className="grid gap-1 text-sm">受让主体<NativeSelect className="w-full" name="grantee_kind" value={granteeKind} onChange={event => setGranteeKind(event.target.value)}><option value="actor">指定账户</option>{props.serviceDevices.length > 0 && <option value="service">已登记的个人 Node 服务</option>}{props.teamGenesis && <option value="members_of">当前团队中指定角色的账户成员</option>}</NativeSelect></label>
          {!props.teamGenesis && <p className="text-xs text-muted-foreground">团队成员选择器需要当前团队已核验并固定的 genesis。</p>}
          {membersOf ? <div className="space-y-3 rounded-lg border border-border p-3">
            <label className="grid gap-1 text-sm">当前团队已固定的完整 genesis<Textarea value={props.teamGenesis ?? ''} readOnly className="font-mono text-xs" /></label>
            <label className="grid gap-1 text-sm">从可信渠道取得并核对的完整 genesis<Input className={`${input} font-mono text-xs`} name="trusted_genesis" pattern={hashPattern} autoComplete="off" spellCheck={false} required /></label>
            <fieldset><legend className="mb-2 text-sm">至少符合一个所选角色</legend><div className="grid gap-1 sm:grid-cols-2">{roleChoices.map(choice => <label key={choice.value} className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="selector_roles" value={choice.value} />{choice.label}</label>)}</div></fieldset>
            <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="genesis_confirmed" required />我已从可信渠道核对该团队的完整 genesis</label>
          </div> : granteeKind === 'service' ? <label className="grid gap-1 text-sm">已核验的 Node 服务<NativeSelect name="grantee_service" className="w-full" required>{props.serviceDevices.map(service => <option key={service.id} value={service.id}>{service.id}</option>)}</NativeSelect></label> : <label className="grid gap-1 text-sm">受让账户 ID<Input className={`${input} font-mono text-xs`} name="grantee_account" pattern={accountPattern} autoComplete="off" spellCheck={false} required /></label>}
          <fieldset><legend className="mb-2 text-sm">允许的操作，默认不选择</legend><div className="grid gap-1 sm:grid-cols-2">{actionChoices.map(choice => <label key={choice.value} className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="actions" value={choice.value} />{choice.label}</label>)}</div></fieldset>
          <label className="grid gap-1 text-sm">执行 Node 的完整公钥<Input className={`${input} font-mono text-xs`} name="audience_node" defaultValue={props.nodePublicKey} pattern="[a-f0-9]{64}" autoComplete="off" spellCheck={false} required /></label>
          <p className="text-xs text-muted-foreground">Node 会再次核验受让主体、执行 Node、项目范围及当前政策。服务授权也必须显式限定操作。</p>
          <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="require_uv" />每次使用都要求 Passkey 身份验证</label>
          <label className="grid gap-1 text-sm">允许继续转授的层数，0 表示禁止转授<Input className={input} name="delegations" type="number" min={0} max={8} step={1} defaultValue={0} required /></label>
          <label className="grid gap-1 text-sm">授权有效期，本地时间<Input className={input} name="expires_at" type="datetime-local" required /></label>
          <Button className={button} type="submit">提交当前项目授权</Button>
        </fieldset></form>
      </CollapsibleContent>
    </Collapsible></Card>

    <Card className="gap-0 py-0"><Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">撤销已有授权</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <form className="space-y-3" onSubmit={event => {
          event.preventDefault(); const data = new FormData(event.currentTarget)
          void run(async () => {
            if (!data.has('revoke_confirmed')) throw new Error('请单独确认撤销该授权。')
            const hash = field(data, 'grant_hash').trim()
            if (!new RegExp(`^${hashPattern}$`).test(hash)) throw new Error('请填写完整的授权 hash。')
            await props.revokeGrant(hash)
          }, '授权撤销结果已核验。')
        }}><fieldset disabled={disabled} className="space-y-3">
          <label className="grid gap-1 text-sm">授权完整 hash<Input className={`${input} font-mono text-xs`} name="grant_hash" pattern={hashPattern} autoComplete="off" spellCheck={false} required /></label>
          <div role="alert" className="space-y-2 rounded-lg border border-destructive/40 p-3 text-sm"><p>此操作会撤销该授权，依赖它继续转授的权限也会失效。</p><label className="flex min-h-11 items-center gap-2"><input type="checkbox" name="revoke_confirmed" required />我确认撤销上述授权及其派生权限</label></div>
          <Button className={button} type="submit" variant="destructive">确认撤销授权</Button>
        </fieldset></form>
      </CollapsibleContent>
    </Collapsible></Card>
  </div>
}
