import { useEffect, useRef, useState } from 'react'
import type { ConnectionEntry, Member, NodeRole, PairReceipt } from '@/lib/node'
import { useDevice } from './context'
import { RecoveryControls } from './recovery-controls'

const button = 'min-h-10 rounded-lg border border-border px-3 py-2 text-sm font-medium hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50'
const input = 'min-h-10 w-full rounded-lg border border-input bg-background px-3 py-2 text-base'
const panel = 'space-y-4 rounded-xl border border-border bg-card p-4'
const roleLabels: Record<NodeRole, string> = {
  gateway: '接入', relay: '中继', replica: '同步', scheduler: '调度', executor: '执行',
}
const nodeRoles: NodeRole[] = ['relay', 'replica', 'gateway', 'scheduler', 'executor']

function entryKey(entry: ConnectionEntry) {
  return `${entry.address}|${entry.node_id}|${entry.relay}`
}

function MemberDetails({ member }: { member: Member }) {
  return <dl className="grid gap-2 text-sm">
    <div><dt className="text-muted-foreground">名称</dt><dd>{member.name || '未命名设备'}</dd></div>
    <div><dt className="text-muted-foreground">类型</dt><dd>{member.kind === 'node' ? '节点' : '客户端'}{member.manage ? ' · 可管理设备' : ''}</dd></div>
    <div><dt className="text-muted-foreground">完整设备指纹</dt><dd className="break-all font-mono">{member.subject_id}</dd></div>
    <div><dt className="text-muted-foreground">公钥</dt><dd className="break-all font-mono">{member.public_key}</dd></div>
    {member.kind === 'node' && <div><dt className="text-muted-foreground">角色</dt><dd>{member.node_roles.map((role) => roleLabels[role]).join('、') || '无'}</dd></div>}
  </dl>
}

export function DeviceControls() {
  const device = useDevice()
  const { ready, identity, entries, directory, pairs } = device
  const [busy, setBusy] = useState(false)
  const actionPending = useRef(false)
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
    if (actionPending.current) return
    actionPending.current = true
    const connection = ready
    setBusy(true)
    setError('')
    setStatus('')
    try {
      await action()
      if (connection?.isCurrent()) setStatus(success)
    } catch (failure: unknown) {
      if (connection?.isCurrent()) setError(failure instanceof Error ? failure.message : '操作失败，请核对后重试。')
    } finally {
      actionPending.current = false
      setBusy(false)
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

  const canManage = ready?.authorization.canManage === true
  const needsReplica = roles.some((role) => role === 'gateway' || role === 'scheduler' || role === 'executor')
  const currentReview = review && pairs.find((pair) => pair.request_id === review.request_id && JSON.stringify(pair) === JSON.stringify(review))

  return <div className="space-y-6">
    <div role="status" className="text-sm text-muted-foreground">{busy ? '正在处理…' : status}</div>
    {error && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error}</p>}

    <section className={panel} aria-labelledby="current-device-title">
      <h2 id="current-device-title" className="font-semibold">当前连接</h2>
      <dl className="grid gap-3 text-sm">
        <div><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono">{directory?.network_id || ready?.scope.space_id || '尚未连接'}</dd></div>
        <div><dt className="text-muted-foreground">此浏览器的设备指纹</dt><dd className="break-all font-mono">{identity?.nodeId || '正在读取'}</dd></div>
        <div><dt className="text-muted-foreground">此浏览器的公钥</dt><dd className="break-all font-mono">{identity?.publicKey || '正在读取'}</dd></div>
        {ready && <>
          <div><dt className="text-muted-foreground">入口地址</dt><dd className="break-all">{ready.entry.address}{ready.entry.relay ? ' · 经中继连接' : ' · 直接连接'}</dd></div>
          <div><dt className="text-muted-foreground">目标节点指纹</dt><dd className="break-all font-mono">{ready.entry.node_id}</dd></div>
        </>}
      </dl>
      <button type="button" className={button} disabled={busy} onClick={() => void run(device.backupVault, '加密设备备份已下载。请另行保存口令。')}>下载加密设备备份</button>
      {ready && <button type="button" className={button} disabled={busy} onClick={() => device.disconnect()}>断开连接</button>}
    </section>

    <section className={panel} aria-labelledby="connection-entries-title">
      <h2 id="connection-entries-title" className="font-semibold">连接入口</h2>
      <p className="text-sm text-muted-foreground">保存你信任的地址和目标节点指纹。切换入口会结束当前连接。</p>
      {entries.length > 0 && <ul className="divide-y divide-border">
        {entries.map((entry) => <li key={entryKey(entry)} className="space-y-2 py-3">
          <p className="break-all text-sm">{entry.address}{entry.relay ? ' · 中继' : ' · 直连'}</p>
          <p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p>
          <div className="flex flex-wrap gap-2">
            <button type="button" className={button} disabled={busy || (ready !== null && entryKey(entry) === entryKey(ready.entry))} onClick={() => void run(() => device.connect(entry))}>
              {ready && entryKey(entry) === entryKey(ready.entry) ? '当前入口' : '连接此入口'}
            </button>
            <button type="button" className={button} disabled={busy} onClick={() => {
              try { device.removeEntry(entry); setStatus('入口已移除。'); setError('') }
              catch (failure: unknown) { setError(failure instanceof Error ? failure.message : '无法移除入口。') }
            }}>移除入口</button>
          </div>
        </li>)}
      </ul>}
      <form className="space-y-3" onSubmit={(event) => {
        event.preventDefault()
        setError('')
        try {
          device.saveEntry({ address: address.trim(), node_id: nodeId.trim(), relay })
          setAddress(''); setNodeId(''); setRelay(false); setStatus('入口已保存。')
        } catch (failure: unknown) { setError(failure instanceof Error ? failure.message : '无法保存入口，请检查地址和指纹。') }
      }}>
        <label className="grid gap-1 text-sm">入口地址<input className={input} type="url" name="entry-address" autoComplete="url" required placeholder="http://192.168.0.99:4790" value={address} onChange={(event) => setAddress(event.target.value)} /></label>
        <label className="grid gap-1 text-sm">目标节点完整指纹<input className={`${input} font-mono`} name="entry-node-id" autoComplete="off" autoCapitalize="none" spellCheck={false} pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备指纹" required value={nodeId} onChange={(event) => setNodeId(event.target.value)} /></label>
        <label className="flex min-h-10 items-center gap-2 text-sm"><input type="checkbox" checked={relay} onChange={(event) => setRelay(event.target.checked)} />通过此入口中继到目标节点</label>
        <button type="submit" className={button} disabled={busy}>保存入口</button>
      </form>
    </section>

    <section className={panel} aria-labelledby="authorized-devices-title">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 id="authorized-devices-title" className="font-semibold">已授权设备</h2>
        <button type="button" className={button} disabled={busy || !ready} onClick={() => void run(() => device.refreshDirectory(), '设备目录已刷新。')}>刷新设备</button>
      </div>
      {directory ? <>
        <p className="text-sm text-muted-foreground">当前授权版本 {directory.epoch}。{canManage ? '此浏览器可以管理普通设备。' : '此浏览器仅可查看授权目录。'}</p>
        <ul className="divide-y divide-border">
          {directory.members.map((member) => <li key={member.subject_id} className="space-y-3 py-4">
            <MemberDetails member={member} />
            {canManage && !member.manage && (revokeSubject === member.subject_id ? <div className="space-y-2 rounded-lg border border-destructive p-3">
              <p className="text-sm">撤销后，此设备将失去网络访问权限。确认撤销「{member.name || member.subject_id}」？</p>
              <div className="flex flex-wrap gap-2">
                <button type="button" className={`${button} text-destructive`} disabled={busy} onClick={() => void run(async () => { await device.revoke(member.subject_id); setRevokeSubject('') }, '设备授权已撤销。')}>确认撤销此设备</button>
                <button type="button" className={button} disabled={busy} onClick={() => setRevokeSubject('')}>取消</button>
              </div>
            </div> : <button type="button" className={button} disabled={busy} onClick={() => setRevokeSubject(member.subject_id)}>撤销设备</button>)}
          </li>)}
        </ul>
        {directory.members.some((member) => member.manage) && <p className="text-sm text-muted-foreground">管理设备的变更需要使用恢复包。</p>}
        {directory.revoked.length > 0 && <details className="text-sm"><summary className="cursor-pointer py-2">已撤销的设备（{directory.revoked.length}）</summary><ul className="space-y-2">{directory.revoked.map((subject) => <li key={subject} className="break-all font-mono text-xs">{subject}</li>)}</ul></details>}
      </> : <p className="text-sm text-muted-foreground">尚未读取设备目录。</p>}
    </section>

    {canManage && <>
      <section className={panel} aria-labelledby="invite-device-title">
        <h2 id="invite-device-title" className="font-semibold">邀请新设备</h2>
        <form className="space-y-3" onSubmit={(event) => {
          event.preventDefault()
          setInvitation('')
          void run(async () => {
            const connection = ready
            const content = await device.createInvitation(inviteKind, inviteKind === 'node' ? roles : [])
            if (connection?.isCurrent()) setInvitation(content)
          }, '邀请已生成，请通过可信渠道发送。')
        }}>
          <fieldset disabled={busy} className="space-y-2">
            <legend className="mb-2 text-sm">设备类型</legend>
            <label className="flex min-h-10 items-center gap-2 text-sm"><input type="radio" name="invite-kind" value="client" checked={inviteKind === 'client'} onChange={() => { setInviteKind('client'); setInvitation('') }} />浏览器或其他客户端</label>
            <label className="flex min-h-10 items-center gap-2 text-sm"><input type="radio" name="invite-kind" value="node" checked={inviteKind === 'node'} onChange={() => { setInviteKind('node'); setInvitation('') }} />运行 Vibeface 的节点</label>
          </fieldset>
          {inviteKind === 'node' && <fieldset disabled={busy} className="space-y-1">
            <legend className="mb-2 text-sm">允许此节点承担的角色</legend>
            {nodeRoles.map((role) => <label key={role} className="flex min-h-10 items-center gap-2 text-sm"><input type="checkbox" checked={roles.includes(role)} disabled={role === 'relay' || (role === 'replica' && needsReplica)} onChange={(event) => { toggleRole(role, event.target.checked); setInvitation('') }} />{roleLabels[role]}{role === 'relay' ? '（节点连接必需）' : role === 'replica' && needsReplica ? '（所选角色必需）' : ''}</label>)}
          </fieldset>}
          <p className="text-sm text-muted-foreground">新设备不会获得设备管理权限。双方仍需核对指纹并确认配对。</p>
          <button type="submit" className={button} disabled={busy}>生成邀请</button>
        </form>
        {invitation && <div className="space-y-2">
          <label className="grid gap-1 text-sm">邀请内容<textarea className={`${input} min-h-28 font-mono text-xs`} readOnly value={invitation} onFocus={(event) => event.target.select()} /></label>
          <p className="text-sm text-muted-foreground">邀请包含配对凭证，仅发送给你信任的设备。</p>
          <button type="button" className={button} disabled={busy} onClick={() => void run(async () => { await navigator.clipboard.writeText(invitation) }, '邀请已复制。')}>复制邀请</button>
          <button type="button" className={`${button} ml-2`} disabled={busy} onClick={() => setInvitation('')}>清除邀请</button>
        </div>}
      </section>

      <section className={panel} aria-labelledby="pair-requests-title">
        <h2 id="pair-requests-title" className="font-semibold">等待配对的设备</h2>
        {pairs.length === 0 ? <p className="text-sm text-muted-foreground">暂无待确认的设备。新设备完成核对后，刷新设备目录查看请求。</p> : <ul className="divide-y divide-border">
          {pairs.map((pair) => <li key={pair.request_id} className="space-y-3 py-4">
            <MemberDetails member={pair.review.new_member} />
            {!pair.new_subject_signature ? <p role="status" className="text-sm text-muted-foreground">等待新设备核对并确认。</p> : currentReview?.request_id === pair.request_id ? <div className="space-y-3 rounded-lg border border-border p-3">
              <p className="text-sm">请与新设备上的配对页面核对完整指纹、类型和角色。</p>
              <label className="flex min-h-10 items-start gap-2 text-sm"><input className="mt-1" type="checkbox" checked={reviewConfirmed} onChange={(event) => setReviewConfirmed(event.target.checked)} />我已通过可信渠道核对以上设备信息</label>
              <div className="flex flex-wrap gap-2">
                <button type="button" className={button} disabled={busy || !reviewConfirmed} onClick={() => void run(async () => { await device.approvePair(pair.request_id); setReview(null); setReviewConfirmed(false) }, '设备已获授权。')}>确认授权此设备</button>
                <button type="button" className={button} disabled={busy} onClick={() => { setReview(null); setReviewConfirmed(false) }}>取消</button>
              </div>
            </div> : <button type="button" className={button} disabled={busy} onClick={() => { setReview(pair); setReviewConfirmed(false) }}>核对设备信息</button>}
          </li>)}
        </ul>}
      </section>
    </>}
    <RecoveryControls />
  </div>
}
