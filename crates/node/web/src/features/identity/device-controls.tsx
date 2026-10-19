import { useEffect, useRef, useState } from 'react'
import { ChevronDown, Plus } from 'lucide-react'
import qrcode from 'qrcode-generator'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import type { ConnectionEntry, NodeRole } from '@/lib/node'
import { record, text } from '@/lib/node'
import { invitationLink } from '@/lib/enrollment-reference'
import { downloadConnectionFile } from '@/lib/connection-file'
import { useDevice } from './context'
import { RecoveryControls } from './recovery-controls'

const button = 'min-h-10'
const input = 'min-h-10'
const disclosure = 'h-auto min-h-14 w-full justify-between gap-3 rounded-none px-5 py-4 text-left whitespace-normal [&[data-state=open]>svg]:rotate-180'
const roleLabels: Record<NodeRole, string> = {
  gateway: '接入', relay: '中继', replica: '同步', scheduler: '调度',
}
const nodeRoles: NodeRole[] = ['relay', 'replica', 'gateway', 'scheduler']
type InvitationShare = { kind: 'client'; content: string; link: string; qr: string | null } | { kind: 'node'; content: string }

function entryKey(entry: ConnectionEntry) {
  return `${entry.address}|${entry.peer_id}|${entry.local === true}`
}

export function DeviceControls({ active }: { active: boolean }) {
  const device = useDevice()
  const { ready, identity, entries, directory } = device
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const actionPending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  const [address, setAddress] = useState('')
  const [peerId, setPeerId] = useState('')
  const [inviteKind, setInviteKind] = useState<'client' | 'node'>('client')
  const [roles, setRoles] = useState<NodeRole[]>(['relay'])
  const [invitation, setInvitation] = useState<InvitationShare | null>(null)

  useEffect(() => {
    setInvitation(null)
    setError('')
    setStatus('')
  }, [ready])

  useEffect(() => {
    if (!ready) return
    let current = true
    const refresh = () => device.refreshDirectory().catch((failure: unknown) => {
      if (current && ready.isCurrent()) setError(failure instanceof Error ? failure.message : '设备目录读取失败，请重试。')
    })
    void refresh()
    const timer = active && invitation ? setInterval(() => { void refresh() }, 5000) : undefined
    return () => { current = false; if (timer !== undefined) clearInterval(timer) }
  }, [ready, active, invitation])

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
      if (next.some((item) => item === 'gateway' || item === 'scheduler') && !next.includes('replica')) next = [...next, 'replica']
      return next
    })
  }

  const canManage = ready?.principal.can_manage === true
  const localInvitation = invitation?.kind === 'client' && ['localhost', '127.0.0.1', '[::1]'].includes(new URL(invitation.link).hostname)
  const needsReplica = roles.some((role) => role === 'gateway' || role === 'scheduler')

  return <div className="space-y-5">
    {(busy || status) && <p role="status" className="text-sm text-muted-foreground">{busy ? '正在处理…' : status}</p>}
    {error && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error}</p>}

    {canManage && <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="flex items-center gap-2 font-semibold"><Plus className="size-4" />邀请新设备</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
          <form className="space-y-4" onSubmit={(event) => {
            event.preventDefault()
            setInvitation(null)
            void run(async () => {
              const connection = ready
              const content = await device.createInvitation(inviteKind, inviteKind === 'node' ? roles : [])
              if (!connection?.isCurrent()) return
              if (inviteKind === 'node') { setInvitation({ kind: 'node', content }); return }
              const connectionInfo: unknown = JSON.parse(content)
              const link = invitationLink(content, text(record(connectionInfo, '连接信息').url, '连接入口'))
              let qr: string | null = null
              try { const code = qrcode(0, 'M'); code.addData(link); code.make(); qr = code.createDataURL(4, 16) } catch { qr = null }
              setInvitation({ kind: 'client', content, link, qr })
            }, '邀请已生成，请通过可信渠道发送。')
          }}>
            <fieldset disabled={busy} className="space-y-2">
              <legend className="mb-2 text-sm font-medium">设备类型</legend>
              <label className="flex min-h-11 items-center gap-2 text-sm"><input type="radio" name="invite-kind" value="client" checked={inviteKind === 'client'} onChange={() => { setInviteKind('client'); setInvitation(null) }} />浏览器或其他客户端</label>
              <label className="flex min-h-11 items-center gap-2 text-sm"><input type="radio" name="invite-kind" value="node" checked={inviteKind === 'node'} onChange={() => { setInviteKind('node'); setInvitation(null) }} />运行 Vibeface 的节点</label>
              {inviteKind === 'node' && <p className="text-sm text-muted-foreground">在对方机器打开该节点的 localhost 网页，选择使用节点邀请加入。双方核对节点身份和权限后确认。</p>}
            </fieldset>
            {inviteKind === 'node' && <fieldset disabled={busy} className="space-y-1">
              <legend className="mb-2 text-sm font-medium">节点角色</legend>
              {nodeRoles.map((role) => <label key={role} className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" checked={roles.includes(role)} disabled={role === 'relay' || (role === 'replica' && needsReplica)} onChange={(event) => { toggleRole(role, event.target.checked); setInvitation(null) }} />{roleLabels[role]}{role === 'relay' ? '（节点连接必需）' : role === 'replica' && needsReplica ? '（所选角色必需）' : ''}</label>)}
            </fieldset>}
            <p className="text-xs text-muted-foreground">新设备不含管理权限。双方仍需核对设备编号并确认配对。</p>
            <p className="text-xs text-muted-foreground">跨机器邀请需先连接到对方可达的节点或中继入口再生成，localhost 仅供本机使用。</p>
            <Button type="submit" className={button} disabled={busy}>生成邀请</Button>
          </form>
          {invitation && <div className="space-y-3 rounded-lg bg-muted/50 p-3">
            {invitation.kind === 'client' && <>
              {invitation.qr ? <img className="h-auto w-80 max-w-full rounded bg-white" src={invitation.qr} alt="在新设备上打开连接邀请的二维码" /> : <p role="status" className="text-sm text-muted-foreground">二维码无法生成，请使用连接链接或文件。</p>}
              <label className="grid gap-2 text-sm">连接链接<Textarea className="min-h-24 font-mono text-xs" readOnly value={invitation.link} onFocus={event => event.target.select()} /></label>
              {localInvitation && <p role="status" className="text-sm text-muted-foreground">此入口只能在这台电脑打开。连接手机前，请配置可访问的公网入口并重新生成邀请。</p>}
            </>}
            <label className="grid gap-2 text-sm">连接信息<Textarea className="min-h-28 font-mono text-xs" readOnly value={invitation.content} onFocus={(event) => event.target.select()} /></label>
            <p className="text-xs text-muted-foreground">{invitation.kind === 'node' ? '连接文件含配对凭证，仅发送给你信任的设备。在对方机器的本机 Web 使用节点邀请，并由双方核对后确认。' : '用新设备扫描二维码、打开链接或导入文件，仍需双方核对并确认。连接信息含配对凭证，仅通过可信渠道发送。'}</p>
            <div className="flex flex-wrap gap-2">
              {invitation.kind === 'client' && <Button type="button" className={button} disabled={busy} onClick={() => void run(async () => { await navigator.clipboard.writeText(invitation.link) }, '连接链接已复制。')}>复制连接链接</Button>}
              <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => void run(async () => { await navigator.clipboard.writeText(invitation.content) }, '连接信息已复制。')}>复制连接信息</Button>
              <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => void run(async () => downloadConnectionFile(invitation.content), '连接文件已下载。')}>下载连接文件</Button>
              <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => setInvitation(null)}>清除</Button>
            </div>
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
              <p className="break-all text-sm">{entry.address}</p>
              <p className="break-all font-mono text-xs text-muted-foreground">{entry.peer_id}</p>
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
              device.saveEntry({ address: address.trim(), peer_id: peerId.trim() })
              setAddress(''); setPeerId(''); setStatus('入口已保存。')
            } catch (failure: unknown) { setError(failure instanceof Error ? failure.message : '无法保存入口，请检查地址和设备编号。') }
          }}>
            <h3 className="text-sm font-medium">添加入口</h3>
            <label className="grid gap-2 text-sm">入口地址<Input className={input} type="url" name="entry-address" autoComplete="url" required placeholder="http://192.168.0.99:4790" value={address} onChange={(event) => setAddress(event.target.value)} /></label>
            <label className="grid gap-2 text-sm">Peer ID<Input className={`${input} font-mono`} name="entry-peer-id" autoComplete="off" autoCapitalize="none" spellCheck={false} pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备编号" required value={peerId} onChange={(event) => setPeerId(event.target.value)} /></label>
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
            <div className="space-y-1"><dt className="text-muted-foreground">此浏览器的设备编号</dt><dd className="break-all font-mono">{identity?.peerId || '正在读取'}</dd></div>
            <div className="space-y-1"><dt className="text-muted-foreground">此浏览器的公钥</dt><dd className="break-all font-mono">{identity?.publicKey || '正在读取'}</dd></div>
            {ready && <>
              <div className="space-y-1"><dt className="text-muted-foreground">入口地址</dt><dd className="break-all">{ready.entry.address}</dd></div>
              <div className="space-y-1"><dt className="text-muted-foreground">Peer ID</dt><dd className="break-all font-mono">{ready.entry.peer_id}</dd></div>
            </>}
          </dl>
          {ready && <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => device.disconnect()}>断开连接</Button>}
        </CollapsibleContent>
      </Collapsible>
    </Card>
    <RecoveryControls />
  </div>
}
