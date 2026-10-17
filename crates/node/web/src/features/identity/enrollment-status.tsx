import { useRef, useState } from 'react'
import type { EnrollmentStatus } from '@/lib/enrollment'
import { messageOf } from '@/lib/node'
import { useDevice } from './context'

const button = 'min-h-11 rounded-lg border border-border px-4 py-2 text-sm font-medium hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50'
const panel = 'space-y-4 rounded-xl border border-border bg-card p-5'
const incomingStatus: Record<EnrollmentStatus, string> = {
  requested: '等待原设备核对完整公钥并批准。',
  approved: '原设备已批准。请继续，使用此浏览器的设备密钥提供持有证明。',
  possessed: '此浏览器已提供密钥持有证明。请回到原设备确认加入账户，再检查结果。',
  completed: '原设备已确认此设备加入账户。',
}
const managerStatus: Record<EnrollmentStatus, string> = {
  requested: '等待你核对新设备公钥并批准。',
  approved: '已批准。请在新设备上继续，提供密钥持有证明后再刷新列表。',
  possessed: '新设备已提供密钥持有证明，等待你确认加入账户。',
  completed: '此设备已加入账户。',
}

export function IncomingEnrollmentControls() {
  const device = useDevice()
  const incoming = device.incomingEnrollment
  const pending = useRef(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [protectLocal, setProtectLocal] = useState(false)
  if (!incoming) return null
  async function run(action: () => Promise<void>) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError('')
    try { await action() }
    catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <section className={panel} aria-labelledby="incoming-enrollment-title" aria-busy={busy}>
    <h2 id="incoming-enrollment-title" className="font-semibold">加入已有账户</h2>
    <p className="text-sm text-muted-foreground">请在原设备上核对下方完整公钥。持有邀请链接仍需原设备明确批准，才可加入账户。</p>
    <dl className="space-y-3 text-xs">
      <div><dt className="text-muted-foreground">账户</dt><dd className="break-all font-mono">{incoming.accountId}</dd></div>
      <div><dt className="text-muted-foreground">此浏览器的新设备完整公钥</dt><dd className="break-all font-mono">{incoming.newDevicePublicKey}</dd></div>
      <div><dt className="text-muted-foreground">申请的账户设备管理权限</dt><dd>{incoming.requestedRights.length > 0 ? <ul className="list-inside list-disc">{incoming.requestedRights.map(right => <li key={right}>{right === 'enroll_device' ? '新增账户设备' : right === 'revoke_device' ? '撤销账户设备' : '未识别的账户管理权限'}</li>)}</ul> : '不申请账户设备管理权限。'}</dd></div>
    </dl>
    <p role="status" className="text-sm">{busy ? '正在检查原设备批准并核对加入结果。' : incomingStatus[incoming.status]}</p>
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    <p className="text-sm text-muted-foreground">原设备批准后，此浏览器提供密钥持有证明。然后回到原设备确认提交，最后在此页面检查结果。</p>
    <div className="flex flex-wrap gap-2">
      {incoming.status !== 'completed' && <button type="button" className={button} disabled={busy} onClick={() => void run(device.refreshIncomingEnrollment)}>检查原设备批准并继续</button>}
      <button type="button" className={button} onClick={device.cancelSensitive}>取消加入</button>
    </div>
    <details className="space-y-3 rounded-lg border border-border p-3"><summary className="cursor-pointer py-2 text-sm font-medium">使用已有账户 Passkey 加入</summary>
      <p className="text-sm text-muted-foreground">使用之前在此账户登记的 Passkey，确认本人身份后授权此设备加入。</p>
      <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" disabled={busy || !device.identity?.profileId} checked={protectLocal} onChange={event => setProtectLocal(event.target.checked)} />也用此 Passkey 解锁本机设备配置</label>
      {!device.identity?.profileId && <p className="text-xs text-muted-foreground">临时会话只验证账户身份。本机保护需要先创建独立口令设备配置。</p>}
      <button type="button" className={button} disabled={busy} onClick={() => void run(() => device.joinWithPasskey(protectLocal))}>验证账户 Passkey 并加入</button>
    </details>
    <p className="text-xs text-muted-foreground">取消只停止本页操作。已经批准或提交的申请，请回到原设备核对结果。</p>
  </section>
}

export function EnrollmentRequestsControls() {
  const device = useDevice()
  const pending = useRef(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  if (!device.managerEnrollmentActive) return null
  async function run(action: () => Promise<unknown>, success: string) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError(''); setStatus('')
    try { await action(); setStatus(success) }
    catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <section className={panel} aria-labelledby="enrollment-requests-title" aria-busy={busy}>
    <div className="flex flex-wrap items-center justify-between gap-3">
      <h2 id="enrollment-requests-title" className="font-semibold">待加入的设备</h2>
      <button type="button" className={button} disabled={busy} onClick={() => void run(device.refreshEnrollmentRequests, '设备申请列表已更新。')}>刷新设备申请</button>
    </div>
    <p className="text-sm text-muted-foreground">批准前，请逐字核对新设备页面显示的完整公钥与下方公钥。邀请链接本身不能授予账户权限。</p>
    <p role="status" className="text-sm text-muted-foreground">{busy ? '正在核对设备申请，请等待节点确认。' : status}</p>
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    {device.enrollmentRequests.length > 0 ? <ul className="divide-y divide-border">{device.enrollmentRequests.map(request => <li key={request.requestId} className="space-y-3 py-4">
      <dl className="space-y-2 text-xs"><div><dt className="text-muted-foreground">新设备完整公钥</dt><dd className="break-all font-mono">{request.newDevicePublicKey}</dd></div></dl>
      <p className="text-sm">{managerStatus[request.status]}</p>
      {request.status === 'requested' && <button type="button" className={button} disabled={busy} onClick={() => void run(() => device.approveEnrollmentRequest(request.requestId), '已批准此设备。请在新设备上继续提供密钥持有证明。')}>核对指纹并批准此设备</button>}
      {request.status === 'possessed' && <button type="button" className={button} disabled={busy} onClick={() => void run(() => device.completeEnrollmentRequest(request.requestId), '已确认此设备加入账户。请在新设备上检查结果。')}>确认加入账户</button>}
    </li>)}</ul> : <p className="text-sm text-muted-foreground">尚无设备申请。请在新设备上打开邀请并提交申请，再刷新此列表。</p>}
  </section>
}
