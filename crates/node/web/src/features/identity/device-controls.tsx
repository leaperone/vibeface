import { useEffect, useRef, useState } from 'react'
import { ChevronDown, Plus } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import type { ConnectionEntry, Member, NodeRole, PairReceipt } from '@/lib/node'
import { useDevice } from './context'
import { RecoveryControls } from './recovery-controls'

const button = 'min-h-10'
const input = 'min-h-10'
const panel = 'gap-4 p-5'
const disclosure = 'h-auto min-h-14 w-full justify-between gap-3 rounded-none px-5 py-4 text-left whitespace-normal [&[data-state=open]>svg]:rotate-180'
const roleLabels: Record<NodeRole, string> = {
  gateway: '接入', relay: '中继', replica: '同步', scheduler: '调度', executor: '执行',
}
const nodeRoles: NodeRole[] = ['relay', 'replica', 'gateway', 'scheduler', 'executor']

function entryKey(entry: ConnectionEntry) {
  return `${entry.address}|${entry.node_id}|${entry.relay}`
}

function MemberDetails({ member, compact = false }: { member: Member; compact?: boolean }) {
  const fingerprints = <dl className="grid gap-3 text-xs">
    <div className="space-y-1"><dt className="text-muted-foreground">完整设备指纹</dt><dd className="break-all font-mono">{member.subject_id}</dd></div>
    <div className="space-y-1"><dt className="text-muted-foreground">公钥</dt><dd className="break-all font-mono">{member.public_key}</dd></div>
  </dl>
  return <div className="min-w-0 space-y-3">
    <div className="flex flex-wrap items-center gap-2">
      <h3 className="break-all text-sm font-medium">{member.name || '未命名设备'}</h3>
      <Badge variant="secondary">{member.kind === 'node' ? '节点' : '客户端'}</Badge>
      {member.manage && <Badge variant="outline">可管理设备</Badge>}
    </div>
    {member.kind === 'node' && <p className="text-xs text-muted-foreground">角色：{member.node_roles.map((role) => roleLabels[role]).join('、') || '无'}</p>}
    {compact ? <Collapsible>
      <CollapsibleTrigger asChild><Button variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-xs text-muted-foreground [&[data-state=open]>svg]:rotate-180" type="button">设备指纹与公钥<ChevronDown className="size-3 transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent className="pt-2">{fingerprints}</CollapsibleContent>
    </Collapsible> : fingerprints}
  </div>
}

export function DeviceControls() {
  const device = useDevice()
  const { ready, identity, entries, directory, pairs } = device
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const actionPending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  const [address, setAddress] = useState('')
  const [nodeId, setNodeId] = useState('')
  const [relay, setRelay] = useState(false)
  const [inviteKind, setInviteKind] = useState<'client' | 'node'>('client')
  const [roles, setRoles] = useState<NodeRole[]>(['relay'])
  const [invitation, setInvitation] = useState('')
  const [review, setReview] = useState<PairReceipt | null>(null)
  const [reviewConfirmed, setReviewConfirmed] = useState(false)
  const [revokeSubject, setRevokeSubject] = useState('')

  useEffect(() => {
    setInvitation('')
    setReview(null)
    setReviewConfirmed(false)
    setRevokeSubject('')
    setError('')
    setStatus('')
    if (!ready) return
    let current = true
    void device.refreshDirectory().catch((failure: unknown) => {
      if (current && ready.isCurrent()) setError(failure instanceof Error ? failure.message : '设备目录读取失败，请重试。')
    })
    return () => { current = false }
  }, [ready])

  async function run(action: () => Promise<void>, success = '') {
    if (actionPending.current === generation) return
    actionPending.current = generation
    const connection = ready
    setBusyGeneration(generation)
    setError('')
    setStatus('')
    try {
      await action()
      if (connection?.isCurrent()) setStatus(success)
    } catch (failure: unknown) {
      if (connection?.isCurrent()) setError(failure instanceof Error ? failure.message : '操作失败，请核对后重试。')
    } finally {
      if (actionPending.current === generation) actionPending.current = null
      setBusyGeneration(owner => owner === generation ? null : owner)
    }
  }

  function toggleRole(role: NodeRole, checked: boolean) {
    setRoles((previous) => {
      if (role === 'relay') return previous
      let next = checked ? [...previous, role] : previous.filter((item) => item !== role)
      if (next.some((item) => item === 'gateway' || item === 'scheduler' || item === 'executor') && !next.includes('replica')) next = [...next, 'replica']
      return next
    })
  }

  const canManage = ready?.principal.can_manage === true
  const needsReplica = roles.some((role) => role === 'gateway' || role === 'scheduler' || role === 'executor')
  const currentReview = review && pairs.find((pair) => pair.request_id === review.request_id && JSON.stringify(pair) === JSON.stringify(review))

  return <div className="space-y-5">
    {(busy || status) && <p role="status" className="text-sm text-muted-foreground">{busy ? '正在处理…' : status}</p>}
    {error && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error}</p>}

    {canManage && pairs.length > 0 && <Card className={panel} aria-labelledby="pair-requests-title">
      <div className="flex flex-wrap items-center gap-2"><h2 id="pair-requests-title" className="font-semibold">待确认配对</h2><Badge variant="secondary">{pairs.length}</Badge></div>
      <ul className="space-y-5">
        {pairs.map((pair) => <li key={pair.request_id} className="space-y-4 rounded-lg bg-muted/40 p-4">
          <MemberDetails member={pair.review.new_member} />
          {!pair.new_subject_signature ? <p role="status" className="text-sm text-muted-foreground">等待新设备核对并确认。</p> : currentReview?.request_id === pair.request_id ? <div className="space-y-3">
            <p className="text-sm">请与新设备核对完整指纹、类型和角色。</p>
            <dl className="grid gap-3 text-xs"><div className="space-y-1"><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono">{pair.review.network_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">邀请管理设备</dt><dd className="break-all font-mono">{pair.review.inviter_subject_id}</dd></div></dl>
            <label className="flex min-h-11 items-start gap-2 text-sm"><input className="mt-1" type="checkbox" checked={reviewConfirmed} onChange={(event) => setReviewConfirmed(event.target.checked)} />我已通过可信渠道核对以上设备信息</label>
            <div className="flex flex-wrap gap-2">
              <Button type="button" className={button} disabled={busy || !reviewConfirmed} onClick={() => void run(async () => { await device.approvePair(pair.request_id); if (ready?.isCurrent()) { setReview(null); setReviewConfirmed(false) } }, '设备已获授权。')}>确认授权</Button>
              <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => { setReview(null); setReviewConfirmed(false) }}>取消</Button>
            </div>
          </div> : <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => { setReview(pair); setReviewConfirmed(false) }}>核对设备信息</Button>}
        </li>)}
      </ul>
    </Card>}

    <Card className={panel} aria-labelledby="authorized-devices-title">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2"><h2 id="authorized-devices-title" className="font-semibold">已授权设备</h2>{directory && <Badge variant="secondary">{directory.members.length}</Badge>}</div>
        <Button type="button" variant="outline" className={button} disabled={busy || !ready} onClick={() => void run(() => device.refreshDirectory(), '设备目录已刷新。')}>刷新</Button>
      </div>
      {directory ? <>
        <ul className="divide-y divide-border">
          {directory.members.map((member) => <li key={member.subject_id} className="space-y-3 py-4 first:pt-0 last:pb-0">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <MemberDetails member={member} compact />
              {canManage && !member.manage && revokeSubject !== member.subject_id && <Button type="button" variant="ghost" className={`${button} text-destructive`} disabled={busy} onClick={() => setRevokeSubject(member.subject_id)}>撤销授权</Button>}
            </div>
            {canManage && !member.manage && revokeSubject === member.subject_id && <div className="space-y-3 rounded-lg border border-destructive/40 bg-destructive/5 p-3">
              <p className="break-all text-sm">撤销后，此设备将失去网络访问权限。确认撤销「{member.name || member.subject_id}」？</p>
              <div className="flex flex-wrap gap-2">
                <Button type="button" variant="destructive" className={button} disabled={busy} onClick={() => void run(async () => { await device.revoke(member.subject_id); if (ready?.isCurrent()) setRevokeSubject('') }, '设备授权已撤销。')}>确认撤销</Button>
                <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => setRevokeSubject('')}>取消</Button>
              </div>
            </div>}
          </li>)}
        </ul>
        {directory.members.some((member) => member.manage) && <p className="text-xs text-muted-foreground">变更管理设备需使用恢复包。</p>}
        {directory.revoked.length > 0 && <Collapsible>
          <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="min-h-10 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">已撤销的设备（{directory.revoked.length}）<ChevronDown className="size-4 transition-transform" /></Button></CollapsibleTrigger>
          <CollapsibleContent><ul className="space-y-2 pt-2">{directory.revoked.map((subject) => <li key={subject} className="break-all font-mono text-xs text-muted-foreground">{subject}</li>)}</ul></CollapsibleContent>
        </Collapsible>}
      </> : <p className="text-sm text-muted-foreground">尚未读取设备目录。</p>}
    </Card>

    {canManage && <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="flex items-center gap-2 font-semibold"><Plus className="size-4" />邀请新设备</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
          <form className="space-y-4" onSubmit={(event) => {
            event.preventDefault()
            setInvitation('')
            void run(async () => {
              const connection = ready
              const content = await device.createInvitation(inviteKind, inviteKind === 'node' ? roles : [])
              if (connection?.isCurrent()) setInvitation(content)
            }, '邀请已生成，请通过可信渠道发送。')
          }}>
            <fieldset disabled={busy} className="space-y-2">
              <legend className="mb-2 text-sm font-medium">设备类型</legend>
              <label className="flex min-h-11 items-center gap-2 text-sm"><input type="radio" name="invite-kind" value="client" checked={inviteKind === 'client'} onChange={() => { setInviteKind('client'); setInvitation('') }} />浏览器或其他客户端</label>
              <label className="flex min-h-11 items-center gap-2 text-sm"><input type="radio" name="invite-kind" value="node" checked={inviteKind === 'node'} onChange={() => { setInviteKind('node'); setInvitation('') }} />运行 Vibeface 的节点</label>
              {inviteKind === 'node' && <p className="text-sm text-muted-foreground">对方要在那台机器运行 <code>vibeface pair</code>，不能用这张网页接受。</p>}
            </fieldset>
            {inviteKind === 'node' && <fieldset disabled={busy} className="space-y-1">
              <legend className="mb-2 text-sm font-medium">节点角色</legend>
              {nodeRoles.map((role) => <label key={role} className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" checked={roles.includes(role)} disabled={role === 'relay' || (role === 'replica' && needsReplica)} onChange={(event) => { toggleRole(role, event.target.checked); setInvitation('') }} />{roleLabels[role]}{role === 'relay' ? '（节点连接必需）' : role === 'replica' && needsReplica ? '（所选角色必需）' : ''}</label>)}
            </fieldset>}
            <p className="text-xs text-muted-foreground">新设备不含管理权限。双方仍需核对指纹并确认配对。</p>
            <Button type="submit" className={button} disabled={busy}>生成邀请</Button>
          </form>
          {invitation && <div className="space-y-3 rounded-lg bg-muted/50 p-3">
            <label className="grid gap-2 text-sm">邀请内容<Textarea className="min-h-28 font-mono text-xs" readOnly value={invitation} onFocus={(event) => event.target.select()} /></label>
            <p className="text-xs text-muted-foreground">{inviteKind === 'node' ? '对方要在那台机器运行 vibeface pair，不能用这张网页接受。邀请含配对凭证，仅发送给你信任的设备。' : '邀请含配对凭证，仅发送给你信任的设备。浏览器邀请可以贴到对方的连接页。'}</p>
            <div className="flex flex-wrap gap-2"><Button type="button" className={button} disabled={busy} onClick={() => void run(async () => { await navigator.clipboard.writeText(invitation) }, '邀请已复制。')}>复制邀请</Button><Button type="button" variant="outline" className={button} disabled={busy} onClick={() => setInvitation('')}>清除</Button></div>
          </div>}
        </CollapsibleContent>
      </Collapsible>
    </Card>}

    <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">连接入口</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-5 px-5 pb-5 data-[state=closed]:hidden">
          <p className="text-xs text-muted-foreground">切换入口会结束当前连接。</p>
          {entries.length > 0 && <ul className="space-y-3">
            {entries.map((entry) => <li key={entryKey(entry)} className="space-y-3 rounded-lg bg-muted/50 p-3">
              <p className="break-all text-sm">{entry.address}<span className="ml-2 text-xs text-muted-foreground">{entry.relay ? '中继' : '直连'}</span></p>
              <p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p>
              <div className="flex flex-wrap gap-2">
                <Button type="button" variant="outline" className={button} disabled={busy || (ready !== null && entryKey(entry) === entryKey(ready.entry))} onClick={() => void run(() => device.connect(entry))}>
                  {ready && entryKey(entry) === entryKey(ready.entry) ? '当前入口' : '连接'}
                </Button>
                <Button type="button" variant="ghost" className={button} disabled={busy} onClick={() => {
                  try { device.removeEntry(entry); setStatus('入口已移除。'); setError('') }
                  catch (failure: unknown) { setError(failure instanceof Error ? failure.message : '无法移除入口。') }
                }}>移除</Button>
              </div>
            </li>)}
          </ul>}
          <form className="space-y-4" onSubmit={(event) => {
            event.preventDefault()
            setError('')
            try {
              device.saveEntry({ address: address.trim(), node_id: nodeId.trim(), relay })
              setAddress(''); setNodeId(''); setRelay(false); setStatus('入口已保存。')
            } catch (failure: unknown) { setError(failure instanceof Error ? failure.message : '无法保存入口，请检查地址和指纹。') }
          }}>
            <h3 className="text-sm font-medium">添加入口</h3>
            <label className="grid gap-2 text-sm">入口地址<Input className={input} type="url" name="entry-address" autoComplete="url" required placeholder="http://192.168.0.99:4790" value={address} onChange={(event) => setAddress(event.target.value)} /></label>
            <label className="grid gap-2 text-sm">目标节点完整指纹<Input className={`${input} font-mono`} name="entry-node-id" autoComplete="off" autoCapitalize="none" spellCheck={false} pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备指纹" required value={nodeId} onChange={(event) => setNodeId(event.target.value)} /></label>
            <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" checked={relay} onChange={(event) => setRelay(event.target.checked)} />通过入口中继到目标节点</label>
            <Button type="submit" className={button} disabled={busy}>保存入口</Button>
          </form>
        </CollapsibleContent>
      </Collapsible>
    </Card>

    <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">当前连接详情</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent className="space-y-4 px-5 pb-5">
          <dl className="grid gap-3 text-xs">
            <div className="space-y-1"><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono">{directory?.network_id || ready?.principal.network_id || '尚未连接'}</dd></div>
            {directory && <div className="space-y-1"><dt className="text-muted-foreground">授权版本</dt><dd>{directory.epoch}</dd></div>}
            <div className="space-y-1"><dt className="text-muted-foreground">此浏览器的设备指纹</dt><dd className="break-all font-mono">{identity?.nodeId || '正在读取'}</dd></div>
            <div className="space-y-1"><dt className="text-muted-foreground">此浏览器的公钥</dt><dd className="break-all font-mono">{identity?.publicKey || '正在读取'}</dd></div>
            {ready && <>
              <div className="space-y-1"><dt className="text-muted-foreground">入口地址</dt><dd className="break-all">{ready.entry.address}{ready.entry.relay ? ' · 经中继连接' : ' · 直接连接'}</dd></div>
              <div className="space-y-1"><dt className="text-muted-foreground">目标节点指纹</dt><dd className="break-all font-mono">{ready.entry.node_id}</dd></div>
            </>}
          </dl>
          {ready && <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => device.disconnect()}>断开连接</Button>}
        </CollapsibleContent>
      </Collapsible>
    </Card>
    <RecoveryControls />
  </div>
}
