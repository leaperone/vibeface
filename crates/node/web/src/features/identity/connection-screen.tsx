import { useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { messageOf, parseEntry } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { useDevice } from './context'
import { VaultControls } from './vault-controls'
import { RecoveryControls, RecoveryReview } from './recovery-controls'

const input = 'min-h-11 w-full rounded-lg border border-input bg-background px-3 py-2 text-base'
const button = 'min-h-11 rounded-lg border border-border px-4 py-2 text-sm font-medium hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50'
const panel = 'space-y-4 rounded-xl border border-border bg-card p-5'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }

export function ConnectionScreen() {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const [error, setError] = useState('')
  const [confirmed, setConfirmed] = useState(false)
  async function run(action: () => Promise<void>) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError('')
    try { await action() } catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  const usable = !!device.identity && !['blocked', 'key_lost', 'initializing'].includes(device.phase)
  function connect(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = event.currentTarget
    const relay = form.elements.namedItem('relay')
    void run(() => device.connect(parseEntry({ address: value(form, 'address'), node_id: value(form, 'node_id'), relay: relay instanceof HTMLInputElement && relay.checked })))
  }
  return <main className="mx-auto min-h-screen w-full max-w-3xl space-y-6 px-4 py-10 sm:px-6">
    <header className="space-y-2"><p className="text-sm font-medium text-muted-foreground">Vibeface</p><h1 className="text-2xl font-semibold tracking-tight">连接自己的设备网络</h1><p className="text-sm text-muted-foreground">先解锁此浏览器的独立设备密钥，再连接已授权空间。账户恢复密钥与设备密钥分别保存。</p></header>
    <div role="status" className="text-sm text-muted-foreground">{device.phase === 'initializing' ? '正在检查加密设备仓库…' : device.phase === 'connecting' ? '正在建立加密连接并核对设备…' : device.phase === 'unpaired' ? '目标设备已核验，等待此浏览器授权。' : '连接不会自动重发业务操作。'}</div>
    {(error || device.error) && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error || device.error}</p>}
    {device.phase === 'blocked' && <section className={panel}><h2 className="font-semibold">需要可信的安全页面</h2><p className="text-sm text-muted-foreground">请在节点本机打开 http://localhost:4790，或使用你信任的 HTTPS 客户端。普通局域网 HTTP 页面不能安全保存设备密钥。可信页面仍可连接允许的局域网节点。</p></section>}
    {['vault_empty', 'vault_locked', 'vault_unavailable', 'key_lost'].includes(device.phase) && <VaultControls />}
    {device.identity && <p className="break-all rounded-lg bg-muted p-3 text-xs text-muted-foreground">此浏览器设备指纹 <span className="font-mono">{device.identity.nodeId}</span></p>}
    {device.identity && <button className={button} type="button" onClick={device.lockVault}>锁定设备仓库</button>}
    {device.review && <RecoveryReview />}
    {device.waiting && <section className={panel} aria-labelledby="pair-review-title">
      <h2 id="pair-review-title" className="font-semibold">{device.waiting.accepted ? '等待管理设备确认' : '确认加入自己的网络'}</h2>
      <p className="text-sm text-muted-foreground">请与管理设备核对以下完整指纹。双方确认后，此浏览器获得普通客户端权限。</p>
      <dl className="space-y-3 text-sm"><div><dt className="text-muted-foreground">个人网络</dt><dd className="break-all font-mono">{device.waiting.network_id}</dd></div><div><dt className="text-muted-foreground">邀请管理设备</dt><dd className="break-all font-mono">{device.waiting.review.inviter_subject_id}</dd></div><div><dt className="text-muted-foreground">此浏览器</dt><dd className="break-all font-mono">{device.waiting.review.new_member.subject_id}</dd></div><div><dt className="text-muted-foreground">设备名称</dt><dd>{device.waiting.review.new_member.name}</dd></div></dl>
      {!device.waiting.accepted && <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" checked={confirmed} onChange={event => setConfirmed(event.target.checked)} />我已核对个人网络及双方设备指纹</label>}
      <div className="flex flex-wrap gap-2"><button className={button} type="button" disabled={busy || !device.waiting.accepted && !confirmed} onClick={() => void run(device.waiting?.accepted ? device.checkPair : device.acceptPair)}>{device.waiting.accepted ? '检查配对进度' : '确认此浏览器加入'}</button><button className={button} type="button" onClick={() => { device.cancelPair(); setConfirmed(false) }}>取消本地配对</button></div>
      <p className="text-xs text-muted-foreground">取消仅清除此浏览器的等待记录。若已发送确认，请在管理设备上核对设备授权。</p>
    </section>}
    <section className={panel} aria-labelledby="connect-title">
      <h2 id="connect-title" className="font-semibold">通过已知入口连接</h2>
      {device.entries.length > 0 && <ul className="space-y-2">{device.entries.map(entry => <li key={`${entry.address}:${entry.node_id}:${entry.relay}`} className="space-y-2 rounded-lg border border-border p-3"><p className="break-all text-sm">{entry.address}{entry.relay ? ' · 中继' : ' · 直连'}</p><p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p><div className="flex gap-2"><button className={button} type="button" disabled={busy || !usable} onClick={() => void run(() => device.connect(entry))}>连接</button><button className={button} type="button" disabled={busy} onClick={() => void run(async () => device.removeEntry(entry))}>移除</button></div></li>)}</ul>}
      <form className="space-y-3" onSubmit={connect}>
        <fieldset disabled={busy || !usable} className="space-y-3">
          <label className="grid gap-1 text-sm">入口地址<input className={input} type="url" name="address" required defaultValue={typeof location === 'undefined' ? '' : location.origin} placeholder="http://192.168.0.99:4790" /></label>
          <label className="grid gap-1 text-sm">目标节点完整指纹<input className={`${input} font-mono`} name="node_id" required pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备指纹" autoComplete="off" autoCapitalize="none" spellCheck={false} /></label>
          <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="relay" />通过该入口中继到目标节点</label>
          <p className="text-xs text-muted-foreground">从目标节点本机终端或可信邀请取得指纹。中继地址与最终目标可以不同。</p>
          <button className={button} type="submit">核对设备并连接</button>
        </fieldset>
      </form>
      {device.entry && <button className={button} type="button" onClick={device.disconnect}>取消并断开</button>}
    </section>
    <section className={panel} aria-labelledby="invitation-title">
      <h2 id="invitation-title" className="font-semibold">初始化或配对新浏览器</h2>
      <p className="text-sm text-muted-foreground">首次创建网络，请在节点终端运行 vibeface access，粘贴其输出。已有网络，请从管理设备取得一次性邀请。</p>
      <form className="space-y-3" onSubmit={event => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          const password = value(form, 'password'); const confirmation = value(form, 'confirmation')
          try { await device.pair(value(form, 'invitation'), value(form, 'name'), password || confirmation ? passphrase(password, confirmation) : undefined) }
          finally { form.reset() }
        })
      }}><fieldset disabled={busy || !usable || !!device.review} className="space-y-3">
        <label className="grid gap-1 text-sm">邀请内容<textarea className={`${input} min-h-28 font-mono text-xs`} name="invitation" required defaultValue={device.initialInvitation} autoComplete="off" spellCheck={false} /></label>
        <label className="grid gap-1 text-sm">此浏览器名称<input className={input} name="name" required maxLength={128} autoComplete="off" /></label>
        <details><summary className="cursor-pointer py-2 text-sm">首次创建网络时设置恢复包口令</summary><div className="space-y-3 pt-2"><p className="text-sm text-muted-foreground">至少 12 个字符。请独立保存口令，网站无法找回。</p><label className="grid gap-1 text-sm">加密恢复包口令<input className={input} type="password" name="password" autoComplete="new-password" minLength={12} /></label><label className="grid gap-1 text-sm">再次填写口令<input className={input} type="password" name="confirmation" autoComplete="new-password" minLength={12} /></label></div></details>
        <button className={button} type="submit">读取邀请并核对授权</button>
      </fieldset></form>
    </section>
    {usable && <RecoveryControls />}
  </main>
}
