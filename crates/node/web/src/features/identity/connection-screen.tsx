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
import { passphrase } from '@/lib/secure'
import { primaryLane } from './connection-lane'
import { StatusOrb } from '@/components/status-orb'
import { useDevice } from './context'
import { RecoveryControls, RecoveryReview } from './recovery-controls'

const input = 'min-h-11'
const button = 'min-h-11'
const panel = 'min-w-0 gap-4 p-4 sm:p-5'
const check = 'size-4 accent-primary focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-50'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }
type Lane = 'invite' | 'saved' | 'manual' | 'recovery' | 'passphrase'
type Purpose = 'empty' | 'bootstrap' | 'pair' | 'node' | 'invalid'

function purposeOf(invitation: string): Purpose {
  const text = invitation.trim()
  if (!text) return 'empty'
  try {
    const parsed = parseInvitation(readInvitation(text))
    return parsed.kind
  } catch (cause) {
    return messageOf(cause).includes('vibeface pair') ? 'node' : 'invalid'
  }
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
  const suggested = primaryLane({ hasInvitation: Boolean(device.initialInvitation), savedCount: device.entries.length })
  const active = lane ?? suggested
  const purpose = purposeOf(invitation)
  const usable = !!device.identity && !['blocked', 'key_lost', 'initializing'].includes(device.phase)
  const linking = busy || ['connecting', 'reconnecting'].includes(device.phase)
  useEffect(() => {
    if (device.initialInvitation) setInvitation(current => current.trim() ? current : device.initialInvitation)
  }, [device.initialInvitation])
  useEffect(() => { setError('') }, [generation])
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
    void run(() => device.connect(parseEntry({ address: value(form, 'address'), node_id: value(form, 'node_id'), relay: relay instanceof HTMLInputElement && relay.checked })))
  }
  function submitInvitation(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = event.currentTarget
    const password = value(form, 'password')
    void run(() => device.pair(invitation, value(form, 'name'), purpose === 'bootstrap' ? passphrase(password, password) : undefined))
  }
  const subtitle = device.review?.kind === 'bootstrap' ? '先保存恢复包，再创建网络。'
    : device.review ? '选择要留下的设备。'
    : device.waiting ? '核对后等待确认。'
    : active === 'saved' && device.entries.length === 1 && ['connecting', 'reconnecting'].includes(device.phase) ? '正在连接。'
    : active === 'saved' ? '选一个入口。'
    : active === 'manual' ? '填写地址和指纹。'
    : active === 'recovery' ? '用恢复包恢复。'
    : active === 'passphrase' ? '只更换口令，不连接。'
    : purpose === 'bootstrap' ? '设置口令后连接。'
    : purpose === 'pair' ? '提交加入请求。'
    : '用邀请加入或创建。'
  const inviteAction = purpose === 'bootstrap' ? '连接并准备创建' : purpose === 'pair' ? '提交加入请求' : '连接'
  const inviteDetail = purpose === 'bootstrap' ? '保存恢复包之后才会创建网络。'
    : purpose === 'pair' ? '还要等管理设备确认。'
    : purpose === 'node' ? '到那台机器运行 vibeface pair，这张网页不能接受。'
    : ''
  const focused = Boolean(device.review || device.waiting)
  const showConnectOrb = linking || ['initializing', 'connecting', 'reconnecting'].includes(device.phase)
  return <main className="h-[var(--app-height,100dvh)] overflow-x-hidden overflow-y-auto overscroll-contain">
    <div className="mx-auto w-full min-w-0 max-w-2xl space-y-5 py-8 pb-[max(2rem,env(safe-area-inset-bottom))] ps-[max(1rem,env(safe-area-inset-left))] pe-[max(1rem,env(safe-area-inset-right))] sm:py-12 sm:ps-[max(1.5rem,env(safe-area-inset-left))] sm:pe-[max(1.5rem,env(safe-area-inset-right))]">
      <header className="space-y-2 pb-2">
        <p className="text-sm font-medium text-muted-foreground">Vibeface</p>
        <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">连接设备</h1>
        <p className="text-sm text-muted-foreground">{subtitle}</p>
      </header>
      {showConnectOrb ? <div className="flex items-center gap-3"><StatusOrb place="connection" size={64} /><p role="status" className="text-sm text-muted-foreground">{device.phase === 'reconnecting' ? '正在重连…' : '正在连接…'}</p></div> : null}
      {device.phase === 'unpaired' && !showConnectOrb ? <p role="status" className="text-sm text-muted-foreground">还没授权。</p> : null}
      {(error || device.error && device.phase !== 'reconnecting') && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error || device.error}</p>}
      {device.phase === 'blocked' && <Card className={panel} aria-labelledby="secure-page-title"><h2 id="secure-page-title" className="font-semibold">需要安全页面</h2><p className="text-sm text-muted-foreground">请用本机或 HTTPS 打开。</p></Card>}
      {device.phase === 'key_lost' && <Card className={panel} aria-labelledby="lost-key-title"><h2 id="lost-key-title" className="font-semibold">密钥已丢失</h2><p className="text-sm text-muted-foreground">新密钥需要重新配对。</p><Button variant="outline" className={`${button} self-start`} type="button" disabled={busy} onClick={() => void run(device.replaceLostKey)}>生成新密钥</Button></Card>}
      {device.review && <RecoveryReview />}
      {device.waiting && <Card className={panel} aria-labelledby="pair-review-title">
        <h2 id="pair-review-title" className="font-semibold">{device.waiting.accepted ? '等待管理设备确认' : '确认加入网络'}</h2>
        <p className="text-sm text-muted-foreground">{device.waiting.accepted ? '等待对方确认。' : '核对指纹后确认。'}</p>
        <dl className="grid gap-3 text-sm"><div className="space-y-1"><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono text-xs">{device.waiting.network_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">邀请管理设备</dt><dd className="break-all font-mono text-xs">{device.waiting.review.inviter_subject_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">此浏览器</dt><dd className="break-all font-mono text-xs">{device.waiting.review.new_member.subject_id}</dd></div><div className="space-y-1"><dt className="text-muted-foreground">设备名称</dt><dd className="break-all">{device.waiting.review.new_member.name}</dd></div></dl>
        {!device.waiting.accepted && <label className="flex min-h-11 items-center gap-2 text-sm"><input className={check} type="checkbox" checked={confirmed} onChange={event => setConfirmed(event.target.checked)} />我已核对个人网络及双方设备指纹</label>}
        <div className="flex flex-wrap gap-2"><Button className={button} type="button" disabled={busy || !device.waiting.accepted && !confirmed} onClick={() => void run(device.waiting?.accepted ? device.checkPair : device.acceptPair)}>{device.waiting.accepted ? '检查配对进度' : '确认加入'}</Button><Button variant="outline" className={button} type="button" onClick={() => { device.cancelPair(); setConfirmed(false) }}>取消本地配对</Button></div>
        <p className="text-xs text-muted-foreground">只取消本机记录。</p>
      </Card>}
      {!focused && active === 'saved' && <Card className={panel} aria-labelledby="saved-entries-title">
        <h2 id="saved-entries-title" className="font-semibold">{device.entries.length === 1 ? '已保存的入口' : '选择已保存的入口'}</h2>
        <ul className="space-y-3">{device.entries.map(entry => <li key={`${entry.address}:${entry.node_id}:${entry.relay}`} className="space-y-2 rounded-lg bg-muted/50 p-3">
          <div className="flex flex-wrap items-center justify-between gap-3"><p className="min-w-0 break-all text-sm">{entry.address}<span className="ml-2 text-xs text-muted-foreground">{entry.relay ? '中继' : '直连'}</span></p><Button className={button} type="button" disabled={linking || !usable} onClick={() => void run(() => device.connect(entry))}>{device.entries.length === 1 && device.phase === 'connecting' ? '正在连接' : '连接'}</Button></div>
          <p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p>
        </li>)}</ul>
        {device.entry && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.disconnect}>取消并断开</Button>}
      </Card>}
      {!focused && active === 'invite' && <Card className={panel} aria-labelledby="invite-title">
        <h2 id="invite-title" className="font-semibold">{purpose === 'bootstrap' ? '准备创建网络' : purpose === 'pair' ? '申请加入网络' : '使用邀请'}</h2>
        <p className="text-sm text-muted-foreground">{purpose === 'bootstrap' ? '口令至少 12 个字符。' : '粘贴邀请。'}</p>
        <form className="space-y-4" onSubmit={submitInvitation}>
          <fieldset disabled={linking || !usable || !!device.review} className="space-y-4 disabled:opacity-70">
            <label className="grid gap-2 text-sm">邀请内容<Textarea className="min-h-28 font-mono text-xs" name="invitation" required value={invitation} aria-invalid={purpose === 'invalid' || undefined} autoComplete="off" spellCheck={false} onChange={event => setInvitation(event.target.value)} /></label>
            <label className="grid gap-2 text-sm">此浏览器名称<Input className={input} name="name" required maxLength={128} autoComplete="off" /></label>
            {purpose === 'bootstrap' && <>
              <label className="grid gap-2 text-sm">加密恢复包口令<Input className={input} type="password" name="password" autoComplete="new-password" minLength={12} required aria-invalid={error.includes('口令') || undefined} /></label>
              <p className="text-xs text-muted-foreground">只填一次，请自行保存。</p>
            </>}
            <Button className={button} type="submit" disabled={purpose === 'node'}>{inviteAction}</Button>
            {inviteDetail ? <p className="text-xs text-muted-foreground">{inviteDetail}</p> : null}
          </fieldset>
        </form>
        {device.entry && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.disconnect}>取消并断开</Button>}
      </Card>}
      {!focused && active === 'manual' && <Card className={panel} aria-labelledby="manual-entry-title">
        <h2 id="manual-entry-title" className="font-semibold">手填入口地址</h2>
        <form className="space-y-4" onSubmit={connect}>
          <fieldset disabled={linking || !usable} className="space-y-4 disabled:opacity-70">
            <label className="grid gap-2 text-sm">入口地址<Input className={input} type="url" name="address" required defaultValue={typeof location === 'undefined' ? '' : location.origin} placeholder="http://192.168.0.99:4790" /></label>
            <label className="grid gap-2 text-sm">目标节点完整指纹<Input className={`${input} font-mono`} name="node_id" required pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备指纹" autoComplete="off" autoCapitalize="none" spellCheck={false} /></label>
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
          {active !== 'saved' && device.entries.length > 0 && <Button variant="ghost" className={button} type="button" onClick={() => setLane('saved')}>已保存的入口</Button>}
          {active !== 'invite' && <Button variant="ghost" className={button} type="button" onClick={() => setLane('invite')}>使用邀请</Button>}
          {active !== 'manual' && <Button variant="ghost" className={button} type="button" onClick={() => setLane('manual')}>手填入口地址</Button>}
          {active !== 'recovery' && usable && <Button variant="ghost" className={button} type="button" onClick={() => setLane('recovery')}>恢复管理权限</Button>}
          {active !== 'passphrase' && usable && <Button variant="ghost" className={button} type="button" onClick={() => setLane('passphrase')}>更换恢复包口令</Button>}
        </div>
      </div>}
      {device.identity && <Collapsible className="px-1">
        <CollapsibleTrigger asChild><Button variant="ghost" className="min-h-11 w-full justify-between text-muted-foreground [&[data-state=open]>svg]:rotate-180" type="button">此浏览器的设备指纹<ChevronDown className="size-4 transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="px-3 pb-3 data-[state=closed]:hidden"><p className="break-all font-mono text-xs text-muted-foreground">{device.identity.nodeId}</p></CollapsibleContent>
      </Collapsible>}
    </div>
  </main>
}
