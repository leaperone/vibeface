import { useEffect, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { readInvitation } from '../../../transport.js'
import { messageOf, parseEntry, parseInvitation } from '@/lib/node'
import { readConnectionFile } from '@/lib/connection-file'
import { passphrase } from '@/lib/secure'
import { primaryLane } from './connection-lane'
import { StatusOrb } from '@/components/status-orb'
import { roleNames } from '@/features/workspace/labels'
import { useDevice } from './context'
import { RecoveryControls, RecoveryReview } from './recovery-controls'

const input = 'min-h-11'
const button = 'min-h-11'
const panel = 'min-w-0 gap-4 p-4 sm:p-5'
const check = 'size-4 accent-primary focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-50'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }
type Lane = 'local' | 'invite' | 'saved' | 'manual' | 'recovery' | 'passphrase'
type Purpose = 'empty' | 'bootstrap' | 'pair' | 'node' | 'invalid'

function purposeOf(invitation: string): Purpose {
  const text = invitation.trim()
  if (!text) return 'empty'
  try {
    const parsed = parseInvitation(readInvitation(text))
    return parsed.kind
  } catch { return 'invalid' }
}

export function ConnectionScreen() {
  const device = useDevice()
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [confirmed, setConfirmed] = useState(false)
  const [invitation, setInvitation] = useState(device.initialInvitation)
  const [lane, setLane] = useState<Lane | null>(null)
  const suggested = primaryLane({ hasInvitation: Boolean(device.initialInvitation), savedCount: device.entries.length, hasLocalSetup: Boolean(device.localSetup) })
  const active = lane ?? suggested
  const purpose = purposeOf(invitation)
  const usable = !!device.identity && !['blocked', 'key_lost', 'initializing'].includes(device.phase)
  const linking = busy || ['connecting', 'reconnecting'].includes(device.phase)
  useEffect(() => {
    if (device.initialInvitation) setInvitation(current => current.trim() ? current : device.initialInvitation)
  }, [device.initialInvitation])
  useEffect(() => { setError('') }, [generation])
  useEffect(() => { setConfirmed(false); if (device.localJoin?.kind === 'joined') setLane('local') }, [device.localJoin])
  function chooseLane(next: Lane) {
    if (!device.entry || !['recovery', 'passphrase'].includes(next)) device.disconnect()
    setLane(next)
  }
  async function run(action: () => Promise<void>) {
    if (pending.current === device.connectionGeneration()) return
    let owner = device.connectionGeneration()
    setError('')
    try {
      const work = action()
      owner = device.connectionGeneration(); pending.current = owner; setBusyGeneration(owner)
      await work
    } catch (cause) { if (owner === device.connectionGeneration()) setError(messageOf(cause)) }
    finally { if (pending.current === owner) pending.current = null; setBusyGeneration(active => active === owner ? null : active) }
  }
  function connect(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = event.currentTarget
    const relay = form.elements.namedItem('relay')
    void run(() => device.connect(parseEntry({ address: value(form, 'address'), peer_id: value(form, 'peer_id'), relay: relay instanceof HTMLInputElement && relay.checked })))
  }
  function submitInvitation(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = event.currentTarget
    const password = value(form, 'password')
    void run(() => device.pair(invitation, value(form, 'name'), purpose === 'bootstrap' ? passphrase(password, password) : undefined))
  }
  function createLocalNetwork(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = event.currentTarget
    void run(async () => {
      const password = value(form, 'password')
      try { await device.createLocalNetwork(value(form, 'name'), passphrase(password, password)) }
      finally { form.reset() }
    })
  }
  async function importConnectionFile(file: File) {
    chooseLane('invite')
    const owner = device.connectionGeneration()
    const content = await readConnectionFile(file)
    if (owner !== device.connectionGeneration()) return
    const parsed = readInvitation(content)
    parseInvitation(parsed)
    setInvitation(JSON.stringify(parsed))
  }
  const subtitle = device.review?.kind === 'bootstrap' ? '先保存恢复包，再创建网络。'
    : device.review ? '选择要留下的设备。'
    : device.waiting ? '核对后等待确认。'
    : device.localJoin?.kind === 'review' ? '核对要加入的个人网络及此节点权限。'
    : device.localJoin?.kind === 'pending' ? '等待原网络的管理设备确认此节点。'
    : device.localJoin?.kind === 'uncertain' ? '先核对加入状态，不自动重发申请。'
    : active === 'local' && device.localSetup?.kind === 'uninitialized' ? '在此浏览器创建个人网络并管理本机节点。'
    : active === 'local' ? '连接本机节点，核验此浏览器的授权。'
    : active === 'saved' && device.entries.length === 1 && ['connecting', 'reconnecting'].includes(device.phase) ? '正在连接。'
    : active === 'saved' ? '选一个入口。'
    : active === 'manual' ? '填写地址和设备编号。'
    : active === 'recovery' ? '用恢复包恢复。'
    : active === 'passphrase' ? '只更换口令，不连接。'
    : purpose === 'bootstrap' ? '设置口令后连接。'
    : purpose === 'pair' ? '提交加入请求。'
    : '用邀请加入或创建。'
  const inviteAction = purpose === 'bootstrap' ? '连接并准备创建' : purpose === 'pair' ? '提交加入请求' : purpose === 'node' ? '申请此节点加入' : '连接'
  const inviteDetail = purpose === 'bootstrap' ? '保存恢复包之后才会创建网络。'
    : purpose === 'pair' ? '还要等管理设备确认。'
    : purpose === 'node' ? '此邀请授权正在运行的本机节点。还需核对权限，并由原网络的管理设备确认。'
    : ''
  const joining = device.localJoin && device.localJoin.kind !== 'idle' && device.localJoin.kind !== 'joined'
  const focused = Boolean(device.review || device.waiting || joining)
  const showConnectOrb = linking || ['initializing', 'connecting', 'reconnecting'].includes(device.phase)
  return <main className="h-[var(--app-height,100dvh)] overflow-x-hidden overflow-y-auto overscroll-contain">
    <div className="mx-auto w-full min-w-0 max-w-2xl space-y-5 py-8 pb-[max(2rem,env(safe-area-inset-bottom))] ps-[max(1rem,env(safe-area-inset-left))] pe-[max(1rem,env(safe-area-inset-right))] sm:py-12 sm:ps-[max(1.5rem,env(safe-area-inset-left))] sm:pe-[max(1.5rem,env(safe-area-inset-right))]">
      <header className="space-y-2 pb-2">
        <p className="text-sm font-medium text-muted-foreground">Vibeface</p>
        <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">{active === 'local' && device.localSetup?.kind === 'uninitialized' ? '设置此节点' : '连接设备'}</h1>
        <p className="text-sm text-muted-foreground">{subtitle}</p>
      </header>
      {showConnectOrb ? <div className="flex items-center gap-3"><StatusOrb place="connection" size={64} /><p role="status" className="text-sm text-muted-foreground">{device.phase === 'reconnecting' ? '正在重连…' : '正在连接…'}</p></div> : null}
      {device.phase === 'unpaired' && !showConnectOrb ? <p role="status" className="text-sm text-muted-foreground">还没授权。</p> : null}
      {(error || device.error && device.phase !== 'reconnecting') && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error || device.error}</p>}
      {device.phase === 'blocked' && <Card className={panel} aria-labelledby="secure-page-title"><h2 id="secure-page-title" className="font-semibold">需要安全页面</h2><p className="text-sm text-muted-foreground">请用本机或 HTTPS 打开。</p></Card>}
      {device.phase === 'key_lost' && <Card className={panel} aria-labelledby="lost-key-title"><h2 id="lost-key-title" className="font-semibold">密钥已丢失</h2><p className="text-sm text-muted-foreground">新密钥需要重新配对。</p><Button variant="outline" className={`${button} self-start`} type="button" disabled={busy} onClick={() => void run(device.replaceLostKey)}>生成新密钥</Button></Card>}
      {device.review && <RecoveryReview />}
      {joining && device.localJoin && <Card className={panel} aria-labelledby="node-join-title">
        <h2 id="node-join-title" className="font-semibold">{device.localJoin.kind === 'review' ? '确认此节点加入网络' : device.localJoin.kind === 'pending' ? '等待管理设备确认此节点' : '核对节点加入状态'}</h2>
        {(device.localJoin.kind === 'review' || device.localJoin.kind === 'pending') && <>
          <dl className="grid gap-3 text-sm"><div className="space-y-1"><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono text-xs">{device.localJoin.review.network_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">邀请管理设备</dt><dd className="break-all font-mono text-xs">{device.localJoin.review.inviter_subject_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">本机节点</dt><dd className="break-all font-mono text-xs">{device.localJoin.review.new_member.subject_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">节点名称</dt><dd>{device.localJoin.review.new_member.name}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">节点权限</dt><dd>{device.localJoin.review.new_member.node_roles.map(role => roleNames[role] ?? role).join('、')}</dd></div></dl>
          {device.localJoin.kind === 'review' && <>
            <label className="flex min-h-11 items-center gap-2 text-sm"><input className={check} type="checkbox" checked={confirmed} onChange={event => setConfirmed(event.target.checked)} />我已核对网络、节点编号及权限</label>
            <Button className={`${button} self-start`} type="button" disabled={busy || !confirmed || device.localJoin.review.expires_at_ms <= Date.now()} onClick={() => void run(device.acceptLocalJoin)}>确认此节点加入</Button>
          </>}
        </>}
        {device.localJoin.kind === 'uncertain' && <p className="text-sm text-muted-foreground">{device.localJoin.message}</p>}
        <p className="text-xs text-muted-foreground">节点加入后，可通过本机访问配置 Agent。添加网络成员仍需管理客户端授权。</p>
        <div className="flex flex-wrap gap-2"><Button className={button} variant="outline" type="button" disabled={busy} onClick={() => void run(device.refreshLocalJoin)}>核对加入状态</Button><Button className={button} variant="ghost" type="button" disabled={busy} onClick={() => { device.useAnotherNodeInvitation(); setLane('invite') }}>返回邀请输入</Button></div>
      </Card>}
      {device.localJoin?.kind === 'joined' && <p role="status" className="text-sm text-muted-foreground">此节点已加入个人网络。可打开本机访问配置此节点。添加网络成员仍需原管理客户端或恢复包授权。</p>}
      {device.waiting && <Card className={panel} aria-labelledby="pair-review-title">
        <h2 id="pair-review-title" className="font-semibold">{device.waiting.accepted ? '等待管理设备确认' : '确认加入网络'}</h2>
        <p className="text-sm text-muted-foreground">{device.waiting.accepted ? '等待对方确认。' : '核对设备编号后确认。'}</p>
        <dl className="grid gap-3 text-sm"><div className="space-y-1"><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono text-xs">{device.waiting.network_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">邀请管理设备</dt><dd className="break-all font-mono text-xs">{device.waiting.review.inviter_subject_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">此浏览器</dt><dd className="break-all font-mono text-xs">{device.waiting.review.new_member.subject_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">设备名称</dt><dd className="break-all">{device.waiting.review.new_member.name}</dd></div></dl>
        {!device.waiting.accepted && <label className="flex min-h-11 items-center gap-2 text-sm"><input className={check} type="checkbox" checked={confirmed} onChange={event => setConfirmed(event.target.checked)} />我已核对个人网络及双方设备编号</label>}
        <div className="flex flex-wrap gap-2"><Button className={button} type="button" disabled={busy || !device.waiting.accepted && !confirmed} onClick={() => void run(device.waiting?.accepted ? device.checkPair : device.acceptPair)}>{device.waiting.accepted ? '检查配对进度' : '确认加入'}</Button><Button variant="outline" className={button} type="button" onClick={() => { device.cancelPair(); setConfirmed(false) }}>取消本地配对</Button></div>
        <p className="text-xs text-muted-foreground">只取消本机记录。</p>
      </Card>}
      {!focused && active === 'local' && device.localSetup && <Card className={panel} aria-labelledby="local-setup-title">
        <h2 id="local-setup-title" className="font-semibold">{device.localSetup.kind === 'uninitialized' ? '创建个人网络' : '连接本机节点'}</h2>
        {device.localSetup.kind === 'uninitialized' ? <>
          <p className="text-sm text-muted-foreground">此节点尚未加入个人网络。先填写浏览器名称并设置恢复包口令，再保存加密恢复包。</p>
          <form className="space-y-4" onSubmit={createLocalNetwork}>
            <fieldset disabled={linking || !usable} className="space-y-4 disabled:opacity-70">
              <label className="grid gap-2 text-sm">此浏览器名称<Input className={input} name="name" required maxLength={128} autoComplete="off" /></label>
              <label className="grid gap-2 text-sm">加密恢复包口令<Input className={input} type="password" name="password" autoComplete="new-password" minLength={12} required aria-describedby="local-passphrase-hint" aria-invalid={error.includes('口令') || undefined} /></label>
              <p id="local-passphrase-hint" className="text-xs text-muted-foreground">至少 12 个字符。请独立保存口令，丢失后无法解锁恢复包。</p>
              <Button className={button} type="submit">准备创建网络</Button>
            </fieldset>
          </form>
          <p className="text-xs text-muted-foreground">已有个人网络时，使用节点邀请加入，或导入恢复包。</p>
          <Button variant="outline" className={`${button} self-start`} type="button" onClick={() => chooseLane('invite')}>使用节点邀请加入</Button>
        </> : <>
          <p className="text-sm text-muted-foreground">{device.localSetup.local_launch ? '此节点已加入个人网络。先核验此浏览器的成员授权，也可打开本机访问配置此节点。' : '此节点已加入个人网络。请使用原有成员的邀请或加密恢复包授权此浏览器。'}</p>
          <p className="break-all text-sm text-muted-foreground">{device.localSetup.entry.address}</p>
          <Button className={`${button} self-start`} type="button" disabled={linking || !usable} onClick={() => { const setup = device.localSetup; if (setup?.kind === 'initialized') void run(() => device.connect(setup.entry)) }}>连接此节点</Button>
          {device.localSetup.local_launch && <>
            <Button className={`${button} self-start`} variant="outline" type="button" disabled={linking || !usable} onClick={() => void run(device.openLocalAccess)}>打开本机访问</Button>
            <p className="text-xs text-muted-foreground">本机访问可配置此节点的 Agent。添加网络成员仍需管理客户端授权。</p>
          </>}
        </>}
        {device.entry && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.disconnect}>取消并断开</Button>}
      </Card>}
      {!focused && active === 'saved' && <Card className={panel} aria-labelledby="saved-entries-title">
        <h2 id="saved-entries-title" className="font-semibold">{device.entries.length === 1 ? '已保存的入口' : '选择已保存的入口'}</h2>
        <ul className="space-y-3">{device.entries.map(entry => <li key={`${entry.address}:${entry.peer_id}:${entry.relay}`} className="space-y-2 rounded-lg bg-muted/50 p-3">
          <div className="flex flex-wrap items-center justify-between gap-3"><p className="min-w-0 break-all text-sm">{entry.address}<span className="ml-2 text-xs text-muted-foreground">{entry.relay ? '中继' : '直连'}</span></p><Button className={button} type="button" disabled={linking || !usable} onClick={() => void run(() => device.connect(entry))}>{device.entries.length === 1 && device.phase === 'connecting' ? '正在连接' : '连接'}</Button></div>
          <p className="break-all font-mono text-xs text-muted-foreground">{entry.peer_id}</p>
        </li>)}</ul>
        {device.entry && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.disconnect}>取消并断开</Button>}
      </Card>}
      {!focused && active === 'invite' && <Card className={panel} aria-labelledby="invite-title">
        <h2 id="invite-title" className="font-semibold">{purpose === 'bootstrap' ? '准备创建网络' : purpose === 'pair' ? '授权此浏览器' : purpose === 'node' ? '将此节点加入网络' : '使用邀请'}</h2>
        <p className="text-sm text-muted-foreground">{purpose === 'bootstrap' ? '口令至少 12 个字符。' : '粘贴连接链接或邀请，也可以导入连接文件。'}</p>
        <form className="space-y-4" onSubmit={submitInvitation}>
          <fieldset disabled={linking || !usable || !!device.review} className="space-y-4 disabled:opacity-70">
            <label className="grid gap-2 text-sm">导入连接文件<Input className={input} type="file" name="connection-file" accept=".json,application/json,text/plain" aria-describedby="connection-file-hint" onChange={event => {
              const file = event.currentTarget.files?.item(0)
              event.currentTarget.value = ''
              if (file) void run(() => importConnectionFile(file))
            }} /></label>
            <p id="connection-file-hint" className="text-xs text-muted-foreground">文件不超过 64 KiB。请从管理设备通过可信渠道取得。</p>
            <label className="grid gap-2 text-sm">连接链接或邀请内容<Textarea className="min-h-28 font-mono text-xs" name="invitation" required value={invitation} aria-invalid={purpose === 'invalid' || undefined} autoComplete="off" spellCheck={false} onChange={event => { if (lane !== 'invite') chooseLane('invite'); setInvitation(event.target.value) }} /></label>
            <label className="grid gap-2 text-sm">{purpose === 'node' ? '此节点名称' : '此浏览器名称'}<Input className={input} name="name" required maxLength={128} autoComplete="off" /></label>
            {purpose === 'bootstrap' && <>
              <label className="grid gap-2 text-sm">加密恢复包口令<Input className={input} type="password" name="password" autoComplete="new-password" minLength={12} required aria-invalid={error.includes('口令') || undefined} /></label>
              <p className="text-xs text-muted-foreground">只填一次，请自行保存。</p>
            </>}
            <Button className={button} type="submit">{inviteAction}</Button>
            {inviteDetail ? <p className="text-xs text-muted-foreground">{inviteDetail}</p> : null}
          </fieldset>
        </form>
        {purpose === 'node' && device.localSetup && <Button variant="outline" className={`${button} self-start`} type="button" disabled={linking || !usable} onClick={() => void run(device.refreshLocalJoin)}>核对已有加入申请</Button>}
        {device.entry && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.disconnect}>取消并断开</Button>}
      </Card>}
      {!focused && active === 'manual' && <Card className={panel} aria-labelledby="manual-entry-title">
        <h2 id="manual-entry-title" className="font-semibold">手填入口地址</h2>
        <form className="space-y-4" onSubmit={connect}>
          <fieldset disabled={linking || !usable} className="space-y-4 disabled:opacity-70">
            <label className="grid gap-2 text-sm">入口地址<Input className={input} type="url" name="address" required defaultValue={typeof location === 'undefined' ? '' : location.origin} placeholder="http://192.168.0.99:4790" /></label>
            <label className="grid gap-2 text-sm">Peer ID<Input className={`${input} font-mono`} name="peer_id" required pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备编号" autoComplete="off" autoCapitalize="none" spellCheck={false} /></label>
            <label className="flex min-h-11 items-center gap-2 text-sm"><input className={check} type="checkbox" name="relay" />通过入口中继到目标节点</label>
            <Button className={button} type="submit">连接</Button>
          </fieldset>
        </form>
        {device.entry && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.disconnect}>取消并断开</Button>}
      </Card>}
      {!focused && active === 'recovery' && usable && <RecoveryControls only="restore" />}
      {!focused && active === 'passphrase' && usable && <RecoveryControls only="passphrase" />}
      {!focused && device.phase !== 'blocked' && device.phase !== 'key_lost' && <div className="space-y-2">
        <p className="text-xs text-muted-foreground">其他方式</p>
        <div className="flex flex-wrap gap-2">
          {active !== 'local' && device.localSetup && <Button variant="ghost" className={button} type="button" onClick={() => chooseLane('local')}>设置此节点</Button>}
          {active !== 'saved' && device.entries.length > 0 && <Button variant="ghost" className={button} type="button" onClick={() => chooseLane('saved')}>已保存的入口</Button>}
          {active !== 'invite' && <Button variant="ghost" className={button} type="button" onClick={() => chooseLane('invite')}>使用邀请</Button>}
          {active !== 'manual' && <Button variant="ghost" className={button} type="button" onClick={() => chooseLane('manual')}>手填入口地址</Button>}
          {active !== 'recovery' && usable && <Button variant="ghost" className={button} type="button" onClick={() => chooseLane('recovery')}>恢复管理权限</Button>}
          {active !== 'passphrase' && usable && <Button variant="ghost" className={button} type="button" onClick={() => chooseLane('passphrase')}>更换恢复包口令</Button>}
        </div>
      </div>}
      {device.identity && <Collapsible className="px-1">
        <CollapsibleTrigger asChild><Button variant="ghost" className="min-h-11 w-full justify-between text-muted-foreground [&[data-state=open]>svg]:rotate-180" type="button">此浏览器的设备编号<ChevronDown className="size-4 transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="px-3 pb-3 data-[state=closed]:hidden"><p className="break-all font-mono text-xs text-muted-foreground">{device.identity.peerId}</p></CollapsibleContent>
      </Collapsible>}
    </div>
  </main>
}
