import { useEffect, useRef, useState } from 'react'
import { entryKey, messageOf } from '@/lib/node'
import { useDevice } from './context'
import { RecoveryControls } from './recovery-controls'
import { EnrollmentQr } from './enrollment-qr'
import { EnrollmentRequestsControls } from './enrollment-status'
import { AccountPasskeyControls } from './account-passkey-controls'

const button = 'min-h-11 rounded-lg border border-border px-3 py-2 text-sm font-medium hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50'
const input = 'min-h-11 w-full rounded-lg border border-input bg-background px-3 py-2 text-base'
const panel = 'space-y-4 rounded-xl border border-border bg-card p-4'
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
  const [relay, setRelay] = useState(false)
  const [enrollmentLink, setEnrollmentLink] = useState('')
  const [manageDevices, setManageDevices] = useState(false)
  const [revokeId, setRevokeId] = useState('')
  const canManageDevices = ready?.authorization.enrollmentRights.some(right => right === 'enroll_device' || right === 'revoke_device') === true

  useEffect(() => {
    setError(''); setStatus(''); setEnrollmentLink(''); setRevokeId('')
    if (!ready || !canManageDevices) return
    let mounted = true
    void device.refreshDirectory().catch(error => { if (mounted && ready.isCurrent()) setError(messageOf(error)) })
    return () => { mounted = false }
  }, [ready, canManageDevices])

  async function run(action: () => Promise<void>, success = '') {
    if (pending.current) return
    pending.current = true; setBusy(true); setError(''); setStatus('')
    const connection = ready
    try { await action(); if (connection?.isCurrent()) setStatus(success) }
    catch (error) { if (connection?.isCurrent()) setError(messageOf(error)) }
    finally { pending.current = false; setBusy(false) }
  }

  return <div className="space-y-6">
    <p role="status" className="text-sm text-muted-foreground">{busy ? '正在核对设备…' : status}</p>
    {device.localProtectionNotice && <p role="status" className="rounded-lg border border-border p-3 text-sm">{device.localProtectionNotice}</p>}
    {error && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error}</p>}
    <section className={panel} aria-labelledby="current-device-title">
      <h2 id="current-device-title" className="font-semibold">当前连接</h2>
      <dl className="grid gap-3 text-sm">
        <div><dt className="text-muted-foreground">账户</dt><dd className="break-all font-mono">{ready?.scope.account_id}</dd></div>
        <div><dt className="text-muted-foreground">空间</dt><dd className="break-all font-mono">{ready?.scope.space_id}</dd></div>
        <div><dt className="text-muted-foreground">项目</dt><dd className="break-all font-mono">{ready?.project.id}</dd></div>
        <div><dt className="text-muted-foreground">浏览器设备证书</dt><dd className="break-all font-mono">{ready?.scope.device_id}</dd></div>
        <div><dt className="text-muted-foreground">浏览器传输指纹</dt><dd className="break-all font-mono">{identity?.nodeId}</dd></div>
        {ready && <><div><dt className="text-muted-foreground">入口地址</dt><dd className="break-all">{ready.entry.address}</dd></div><div><dt className="text-muted-foreground">目标 Node 指纹</dt><dd className="break-all font-mono">{ready.entry.node_id}</dd></div></>}
      </dl>
      <button type="button" className={button} disabled={busy} onClick={() => void run(device.backupVault, '设备密钥备份已下载。重新连接仍需可信账户信息，完整账户恢复请使用单独的恢复包。')}>下载设备密钥备份</button>
      <button type="button" className={button} disabled={busy} onClick={device.disconnect}>断开连接</button>
    </section>
    <section className={panel} aria-labelledby="connection-entries-title">
      <h2 id="connection-entries-title" className="font-semibold">连接入口</h2>
      <p className="text-sm text-muted-foreground">保存你信任的地址和 Node 指纹。切换入口会结束当前连接。</p>
      {entries.length > 0 && <ul className="divide-y divide-border">{entries.map(entry => <li key={entryKey(entry)} className="space-y-2 py-3"><p className="break-all text-sm">{entry.address}{entry.relay ? ' · 中继' : ' · 直连'}</p><p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p><div className="flex flex-wrap gap-2"><button type="button" className={button} disabled={busy || !!ready && entryKey(entry) === entryKey(ready.entry)} onClick={() => void run(() => device.connect(entry))}>连接此入口</button><button type="button" className={button} disabled={busy} onClick={() => void run(async () => device.removeEntry(entry), '入口已移除。')}>移除入口</button></div></li>)}</ul>}
      <form className="space-y-3" onSubmit={event => {
        event.preventDefault()
        void run(async () => { device.saveEntry({ address: address.trim(), node_id: nodeId.trim(), relay }); setAddress(''); setNodeId(''); setRelay(false) }, '入口已保存。')
      }}><fieldset disabled={busy} className="space-y-3">
        <label className="grid gap-1 text-sm">入口地址<input className={input} type="url" required autoComplete="url" value={address} onChange={event => setAddress(event.target.value)} /></label>
        <label className="grid gap-1 text-sm">Node 完整指纹<input className={`${input} font-mono`} required pattern="[a-f0-9]{64}" autoComplete="off" autoCapitalize="none" spellCheck={false} value={nodeId} onChange={event => setNodeId(event.target.value)} /></label>
        <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" checked={relay} onChange={event => setRelay(event.target.checked)} />通过该入口中继到目标 Node</label>
        <button type="submit" className={button}>保存入口</button>
      </fieldset></form>
    </section>
    {canManageDevices && <section className={panel} aria-labelledby="account-devices-title">
      <div className="flex flex-wrap items-center justify-between gap-3"><h2 id="account-devices-title" className="font-semibold">账户已授权设备</h2><button type="button" className={button} disabled={busy} onClick={() => void run(device.refreshDirectory, '账户设备已重新核验。')}>刷新设备</button></div>
      <p className="text-sm text-muted-foreground">显示当前账户仍登记的设备证书及其有效状态。空间成员管理权限不等于账户设备管理权限。</p>
      {ready?.authorization.enrollmentRights.includes('enroll_device') && <div className="space-y-3 rounded-lg border border-border p-3">
        <h3 className="text-sm font-medium">新增自己的设备</h3>
        <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" disabled={busy} checked={manageDevices} onChange={event => setManageDevices(event.target.checked)} />允许新设备新增和撤销其他设备</label>
        <p className="text-xs text-muted-foreground">扫码后仍需在此设备上核对新设备指纹并批准。链接到期后需要重新生成。</p>
        <button type="button" className={button} disabled={busy} onClick={() => void run(async () => { const link = await device.createEnrollmentOffer(manageDevices); if (ready?.isCurrent()) setEnrollmentLink(link) })}>生成设备邀请二维码</button>
        {enrollmentLink && <EnrollmentQr link={enrollmentLink} />}
      </div>}
      {directory ? <ul className="divide-y divide-border">{directory.devices.map(item => <li key={item.device_id} className="space-y-2 py-4"><p className="text-sm">{item.subject.kind === 'service' ? 'Node 服务设备' : '账户客户端'}</p><p className="break-all font-mono text-xs">{item.device_id}</p><dl className="space-y-2 text-xs"><div><dt className="text-muted-foreground">公钥</dt><dd className="break-all font-mono">{item.public_key}</dd></div><div><dt className="text-muted-foreground">证书状态</dt><dd>{deviceStatus[item.status]}</dd></div></dl>{ready?.authorization.enrollmentRights.includes('revoke_device') && item.status === 'active' && <button type="button" className={button} disabled={busy} onClick={() => setRevokeId(item.device_id)}>撤销此设备</button>}</li>)}</ul> : <p className="text-sm text-muted-foreground">尚未取得可验证的设备目录。</p>}
      {revokeId && <div role="alert" className="space-y-3 rounded-lg border border-destructive p-3 text-sm"><p>确认撤销以下设备的账户权限。该设备将无法继续使用原证书。</p><p className="break-all font-mono text-xs">{revokeId}</p>{revokeId === ready?.scope.device_id && <p>这是当前浏览器设备。撤销后会锁定当前配置。</p>}<div className="flex flex-wrap gap-2"><button type="button" className={button} disabled={busy} onClick={() => void run(async () => { await device.revokeDevice(revokeId); setRevokeId('') }, '设备权限已撤销。')}>确认撤销</button><button type="button" className={button} disabled={busy} onClick={() => setRevokeId('')}>保留设备</button></div></div>}
    </section>}
    <EnrollmentRequestsControls />
    <AccountPasskeyControls />
    <RecoveryControls />
  </div>
}
