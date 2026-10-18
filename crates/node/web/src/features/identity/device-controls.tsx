import { useEffect, useRef, useState } from 'react'
import { ChevronDown, Plus } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { readInvitation } from '../../../transport.js'
import { entryKey, messageOf, parseGatewayConnectionCard } from '@/lib/node'
import { useDevice } from './context'
import { RecoveryControls } from './recovery-controls'
import { EnrollmentQr } from './enrollment-qr'
import { EnrollmentRequestsControls } from './enrollment-status'
import { AccountPasskeyControls } from './account-passkey-controls'
import { NodeControls } from './node-controls'
import { TeamWorkspaceControls } from './team-workspace-controls'
import { GatewayControls } from './gateway-controls'
import { LocalAccessControls } from './local-access-controls'

const button = 'min-h-10'
const input = 'min-h-10'
const panel = 'gap-4 p-5'
const disclosure = 'h-auto min-h-14 w-full justify-between gap-3 rounded-none px-5 py-4 text-left whitespace-normal [&[data-state=open]>svg]:rotate-180'
const deviceStatus = { active: '有效', expired: '已过期', pending: '尚未生效' }

export function DeviceControls() {
  const device = useDevice()
  const { ready, identity, entries, directory } = device
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  const [address, setAddress] = useState('')
  const [nodeId, setNodeId] = useState('')
  const [gatewayCard, setGatewayCard] = useState('')
  const [relay, setRelay] = useState(false)
  const [enrollmentLink, setEnrollmentLink] = useState('')
  const [manageDevices, setManageDevices] = useState(false)
  const [revokeId, setRevokeId] = useState('')
  let gatewayPreview: ReturnType<typeof parseGatewayConnectionCard> | null = null
  try { if (gatewayCard.trim()) gatewayPreview = parseGatewayConnectionCard(readInvitation(gatewayCard)) } catch {}
  const canManageDevices = ready?.authorization.enrollmentRights.some(right => right === 'enroll_device' || right === 'revoke_device') === true

  useEffect(() => {
    setError(''); setStatus(''); setEnrollmentLink(''); setRevokeId('')
    if (!ready || !canManageDevices) return
    let mounted = true
    void device.refreshDirectory().catch(error => { if (mounted && ready.isCurrent()) setError(messageOf(error)) })
    return () => { mounted = false }
  }, [ready, canManageDevices])

  async function run(action: () => Promise<void>, success = '', connectionMayChange = false) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError(''); setStatus('')
    const connection = ready
    try { await action(); if (connectionMayChange || connection?.isCurrent()) setStatus(success) }
    catch (error) { if (connectionMayChange || connection?.isCurrent()) setError(messageOf(error)) }
    finally { pending.current = false; setBusy(false) }
  }

  return <div className="space-y-5">
    <LocalAccessControls />
    {(busy || status) && <p role="status" className="text-sm text-muted-foreground">{busy ? '正在核对设备…' : status}</p>}
    {device.localProtectionNotice && <p role="status" className="rounded-lg border border-border p-3 text-sm">{device.localProtectionNotice}</p>}
    {error && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error}</p>}

    <EnrollmentRequestsControls />

    <GatewayControls />

    {canManageDevices && <Card className={panel} aria-labelledby="account-devices-title">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2"><h2 id="account-devices-title" className="font-semibold">账户已授权设备</h2>{directory && <Badge variant="secondary">{directory.devices.length}</Badge>}</div>
        <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => void run(device.refreshDirectory, '账户设备已重新核验。')}>刷新设备</Button>
      </div>
      <p className="text-sm text-muted-foreground">显示当前账户仍登记的设备证书及其有效状态。空间成员管理权限不等于账户设备管理权限。</p>
      {directory ? <ul className="divide-y divide-border">
        {directory.devices.map(item => <li key={item.device_id} className="space-y-3 py-4 first:pt-0 last:pb-0">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0 space-y-2">
              <div className="flex flex-wrap items-center gap-2">
                <h3 className="text-sm font-medium">{item.subject.kind === 'service' ? 'Node 服务设备' : '账户客户端'}</h3>
                <Badge variant={item.status === 'active' ? 'secondary' : 'outline'}>{deviceStatus[item.status]}</Badge>
              </div>
              <Collapsible>
                <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-xs text-muted-foreground [&[data-state=open]>svg]:rotate-180">设备证书与公钥<ChevronDown className="size-3 transition-transform" /></Button></CollapsibleTrigger>
                <CollapsibleContent className="pt-2">
                  <dl className="grid gap-3 text-xs">
                    <div className="space-y-1"><dt className="text-muted-foreground">设备证书</dt><dd className="break-all font-mono">{item.device_id}</dd></div>
                    <div className="space-y-1"><dt className="text-muted-foreground">公钥</dt><dd className="break-all font-mono">{item.public_key}</dd></div>
                  </dl>
                </CollapsibleContent>
              </Collapsible>
            </div>
            {ready?.authorization.enrollmentRights.includes('revoke_device') && item.status === 'active' && <Button type="button" variant="ghost" className={`${button} text-destructive`} disabled={busy} onClick={() => setRevokeId(item.device_id)}>撤销此设备</Button>}
          </div>
        </li>)}
      </ul> : <p className="text-sm text-muted-foreground">尚未取得可验证的设备目录。</p>}
      {revokeId && <div role="alert" className="space-y-3 rounded-lg border border-destructive/40 bg-destructive/5 p-3 text-sm">
        <p>确认撤销以下设备的账户权限。该设备将无法继续使用原证书。</p>
        <p className="break-all font-mono text-xs">{revokeId}</p>
        {revokeId === ready?.scope.device_id && <p>这是当前浏览器设备。撤销后会锁定当前配置。</p>}
        <div className="flex flex-wrap gap-2">
          <Button type="button" variant="destructive" className={button} disabled={busy} onClick={() => void run(async () => { await device.revokeDevice(revokeId); setRevokeId('') }, '设备权限已撤销。')}>确认撤销</Button>
          <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => setRevokeId('')}>保留设备</Button>
        </div>
      </div>}
    </Card>}

    {ready?.authorization.enrollmentRights.includes('enroll_device') && <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="flex items-center gap-2 font-semibold"><Plus className="size-4" />新增自己的设备</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
          <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" disabled={busy} checked={manageDevices} onChange={event => setManageDevices(event.target.checked)} />允许新设备新增和撤销其他设备</label>
          <p className="text-xs text-muted-foreground">扫码后仍需在此设备上核对新设备指纹并批准。链接到期后需要重新生成。</p>
          <Button type="button" className={button} disabled={busy} onClick={() => void run(async () => { const link = await device.createEnrollmentOffer(manageDevices); if (ready?.isCurrent()) setEnrollmentLink(link) })}>生成设备邀请二维码</Button>
          {enrollmentLink && <EnrollmentQr link={enrollmentLink} />}
        </CollapsibleContent>
      </Collapsible>
    </Card>}

    <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">连接入口</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-5 px-5 pb-5 data-[state=closed]:hidden">
          <p className="text-xs text-muted-foreground">保存你信任的地址和 Node 指纹。切换入口会结束当前连接。</p>
          {entries.length > 0 && <ul className="space-y-3">
            {entries.map(entry => <li key={entryKey(entry)} className="space-y-3 rounded-lg bg-muted/50 p-3">
              <p className="break-all text-sm">{entry.address}<span className="ml-2 text-xs text-muted-foreground">{entry.relay ? '中继' : '直连'}</span></p>
              <p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p>
              <div className="flex flex-wrap gap-2">
                <Button type="button" variant="outline" className={button} disabled={busy || (ready !== null && entryKey(entry) === entryKey(ready.entry))} onClick={() => void run(() => device.connect(entry))}>{ready && entryKey(entry) === entryKey(ready.entry) ? '当前入口' : '连接此入口'}</Button>
                <Button type="button" variant="ghost" className={button} disabled={busy} onClick={() => void run(async () => device.removeEntry(entry), '入口已移除。')}>移除入口</Button>
              </div>
            </li>)}
          </ul>}
          {ready && <section aria-labelledby="gateway-connection-title" className="space-y-3 rounded-lg border border-border p-3">
            <h3 id="gateway-connection-title" className="text-sm font-medium">通过网关访问此项目</h3>
            <p className="text-xs text-muted-foreground">项目保留在当前 Node。网关仅转发你为此项目签名的业务请求。</p>
            <dl className="grid gap-2 text-xs">
              <div><dt className="text-muted-foreground">当前项目</dt><dd className="break-all font-mono">{ready.project.id}</dd></div>
              <div><dt className="text-muted-foreground">项目 Node 指纹</dt><dd className="break-all font-mono">{ready.entry.node_id}</dd></div>
            </dl>
            {ready.gateway && <div role="status" className="space-y-2 rounded-md bg-muted/50 p-3 text-sm">
              <p>{ready.gateway.status === 'ready' ? '业务请求已通过网关' : ready.gateway.status === 'connecting' ? '正在连接网关…' : '网关不可用'}</p>
              <p className="break-all text-xs">{ready.gateway.entry.address} → 当前项目</p>
              <p className="break-all font-mono text-xs text-muted-foreground">{ready.gateway.entry.node_id}</p>
              {ready.gateway.error && <p className="text-xs text-destructive">{ready.gateway.error}</p>}
              <div className="flex flex-wrap gap-2">
                <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => void run(() => device.connectGatewayCard(''), '网关已重新连接。', true)}>重新连接网关</Button>
                <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => void run(async () => device.disconnectGateway(), '业务请求已恢复直接连接。')}>恢复直接连接</Button>
              </div>
            </div>}
            <form className="space-y-3" onSubmit={event => { event.preventDefault(); void run(() => device.connectGatewayCard(gatewayCard), '网关已启用。', true) }}>
              <label className="grid gap-2 text-sm">受信网关连接卡<Textarea name="gateway-card" required rows={4} spellCheck={false} autoCapitalize="none" autoComplete="off" disabled={busy} value={gatewayCard} onChange={event => setGatewayCard(event.target.value)} placeholder="粘贴包含网关地址、指纹和已批准路由的连接卡" /></label>
              {gatewayPreview && <div className="space-y-1 text-xs"><p className="break-all">网关地址 {gatewayPreview.gateway.entry.address}</p><p className="break-all font-mono text-muted-foreground">网关指纹 {gatewayPreview.gateway.entry.node_id}</p></div>}
              <Button type="submit" className={button} disabled={busy || !gatewayCard.trim()}>启用网关</Button>
            </form>
          </section>}
          <form className="space-y-4" onSubmit={event => {
            event.preventDefault()
            void run(async () => { device.saveEntry({ address: address.trim(), node_id: nodeId.trim(), relay }); setAddress(''); setNodeId(''); setRelay(false) }, '入口已保存。')
          }}>
            <fieldset disabled={busy} className="space-y-3">
              <legend className="mb-2 text-sm font-medium">添加入口</legend>
              <label className="grid gap-2 text-sm">入口地址<Input className={input} type="url" name="entry-address" required autoComplete="url" value={address} onChange={event => setAddress(event.target.value)} /></label>
              <label className="grid gap-2 text-sm">Node 完整指纹<Input className={`${input} font-mono`} name="entry-node-id" required pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备指纹" autoComplete="off" autoCapitalize="none" spellCheck={false} value={nodeId} onChange={event => setNodeId(event.target.value)} /></label>
              <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" checked={relay} onChange={event => setRelay(event.target.checked)} />通过该入口中继到目标 Node</label>
              <Button type="submit" className={button}>保存入口</Button>
            </fieldset>
          </form>
        </CollapsibleContent>
      </Collapsible>
    </Card>

    <Card className="gap-0 py-0">
      <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" className={disclosure}><span className="font-semibold">当前连接详情</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent className="space-y-4 px-5 pb-5">
          <dl className="grid gap-3 text-xs">
            <div className="space-y-1"><dt className="text-muted-foreground">账户</dt><dd className="break-all font-mono">{ready?.scope.account_id}</dd></div>
            <div className="space-y-1"><dt className="text-muted-foreground">空间</dt><dd className="break-all font-mono">{ready?.scope.space_id}</dd></div>
            <div className="space-y-1"><dt className="text-muted-foreground">项目</dt><dd className="break-all font-mono">{ready?.project.id}</dd></div>
            {ready?.gateway && <div className="space-y-1"><dt className="text-muted-foreground">业务网关</dt><dd className="break-all">{ready.gateway.entry.address} → {ready.entry.address}</dd></div>}
            <div className="space-y-1"><dt className="text-muted-foreground">浏览器设备证书</dt><dd className="break-all font-mono">{ready?.scope.device_id}</dd></div>
            <div className="space-y-1"><dt className="text-muted-foreground">浏览器传输指纹</dt><dd className="break-all font-mono">{identity?.nodeId}</dd></div>
            <div className="space-y-1"><dt className="text-muted-foreground">浏览器传输公钥</dt><dd className="break-all font-mono">{identity?.publicKey}</dd></div>
            {ready && <>
              <div className="space-y-1"><dt className="text-muted-foreground">入口地址</dt><dd className="break-all">{ready.entry.address}{ready.entry.relay ? ' · 经中继连接' : ' · 直接连接'}</dd></div>
              <div className="space-y-1"><dt className="text-muted-foreground">目标 Node 指纹</dt><dd className="break-all font-mono">{ready.entry.node_id}</dd></div>
            </>}
          </dl>
          <div className="flex flex-wrap gap-2">
            <Button type="button" variant="outline" className={button} disabled={busy} onClick={() => void run(device.backupVault, '设备密钥备份已下载。重新连接仍需可信账户信息，完整账户恢复请使用单独的恢复包。')}>下载设备密钥备份</Button>
            <Button type="button" variant="outline" className={button} disabled={busy} onClick={device.disconnect}>断开连接</Button>
          </div>
        </CollapsibleContent>
      </Collapsible>
    </Card>
    <NodeControls />
    <TeamWorkspaceControls />
    <AccountPasskeyControls />
    <RecoveryControls />
  </div>
}
