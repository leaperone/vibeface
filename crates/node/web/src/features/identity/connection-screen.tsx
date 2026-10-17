import { useEffect, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { messageOf, parseEntry } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { useDevice } from './context'
import { RecoveryControls, RecoveryReview } from './recovery-controls'
import { VaultControls } from './vault-controls'
import { IncomingEnrollmentControls } from './enrollment-status'

const input = 'min-h-10'
const button = 'min-h-10'
const panel = 'gap-4 p-5'
const disclosure = 'group h-auto min-h-14 w-full justify-between gap-3 rounded-none px-5 py-4 text-left whitespace-normal [&[data-state=open]>svg]:rotate-180'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }

export function ConnectionScreen() {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const [error, setError] = useState('')
  const [entryOpen, setEntryOpen] = useState(false)
  const [invitationOpen, setInvitationOpen] = useState(false)
  useEffect(() => {
    setEntryOpen(device.entries.length === 0 && !device.initialInvitation)
    if (device.initialInvitation) setInvitationOpen(true)
  }, [device.entries.length, device.initialInvitation])
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
  return <main className="h-dvh overflow-y-auto overscroll-contain">
    <div className="mx-auto w-full max-w-2xl space-y-5 px-4 py-8 pb-[max(2rem,env(safe-area-inset-bottom))] sm:px-6 sm:py-12">
      <header className="space-y-2 pb-2">
        <p className="text-sm font-medium text-muted-foreground">Vibeface</p>
        <h1 className="text-2xl font-semibold tracking-tight">连接设备</h1>
        <p className="text-sm text-muted-foreground">解锁此浏览器的独立设备配置，再选择入口或使用设备邀请。</p>
      </header>
      {['initializing', 'connecting', 'unpaired'].includes(device.phase) && <p role="status" className="text-sm text-muted-foreground">{device.phase === 'initializing' ? '正在读取设备密钥…' : device.phase === 'connecting' ? '正在建立加密连接并核对设备…' : '等待此浏览器授权。'}</p>}
      {(error || device.error) && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error || device.error}</p>}
      {device.phase === 'blocked' && <Card className={panel} aria-labelledby="secure-page-title"><h2 id="secure-page-title" className="font-semibold">需要可信的安全页面</h2><p className="text-sm text-muted-foreground">请在节点本机打开 http://localhost:4790，或使用你信任的 HTTPS 客户端。普通局域网 HTTP 页面不能安全保存设备密钥。可信页面仍可连接允许的局域网节点。</p></Card>}
      {['vault_empty', 'vault_locked', 'vault_unavailable', 'key_lost'].includes(device.phase) && <VaultControls />}
      {device.identity && <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.lockVault}>锁定设备仓库</Button>}
      {device.review && <RecoveryReview />}
      <IncomingEnrollmentControls />
      <Card className="gap-0 py-0">
        {device.entries.length > 0 && <div className="space-y-3 p-5">
          <h2 className="font-semibold">已保存的入口</h2>
          <ul className="space-y-3">{device.entries.map(entry => <li key={`${entry.address}:${entry.node_id}:${entry.relay}`} className="space-y-2 rounded-lg bg-muted/50 p-3">
            <div className="flex flex-wrap items-center justify-between gap-3"><p className="min-w-0 break-all text-sm">{entry.address}<span className="ml-2 text-xs text-muted-foreground">{entry.relay ? '中继' : '直连'}</span></p><div className="flex gap-2"><Button className={button} type="button" disabled={busy || !usable} onClick={() => void run(() => device.connect(entry))}>连接</Button><Button variant="ghost" className={button} type="button" disabled={busy} onClick={() => void run(async () => device.removeEntry(entry))}>移除</Button></div></div>
            <p className="break-all font-mono text-xs text-muted-foreground">{entry.node_id}</p>
          </li>)}</ul>
        </div>}
        <Collapsible open={entryOpen} onOpenChange={setEntryOpen}>
          <CollapsibleTrigger asChild><Button variant="ghost" className={disclosure} type="button"><span className="font-semibold">{device.entries.length ? '添加连接入口' : '通过入口连接'}</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
          <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
            <form className="space-y-4" onSubmit={connect}>
              <fieldset disabled={busy || !usable} className="space-y-4">
                <label className="grid gap-2 text-sm">入口地址<Input className={input} type="url" name="address" required defaultValue={typeof location === 'undefined' ? '' : location.origin} placeholder="http://192.168.0.99:4790" /></label>
                <label className="grid gap-2 text-sm">目标节点完整指纹<Input className={`${input} font-mono`} name="node_id" required pattern="[a-f0-9]{64}" title="填写 64 位小写十六进制设备指纹" autoComplete="off" autoCapitalize="none" spellCheck={false} /></label>
                <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="relay" />通过入口中继到目标节点</label>
                <p className="text-xs text-muted-foreground">从目标节点终端或可信邀请取得指纹。中继地址与最终目标可以不同。</p>
                <Button className={button} type="submit">核对并连接</Button>
              </fieldset>
            </form>
          </CollapsibleContent>
        </Collapsible>
        {device.entry && <div className="px-5 pb-5"><Button variant="outline" className={button} type="button" onClick={device.disconnect}>取消并断开</Button></div>}
      </Card>
      <Card className="gap-0 py-0">
        <Collapsible open={invitationOpen} onOpenChange={setInvitationOpen}>
          <CollapsibleTrigger asChild><Button variant="ghost" className={disclosure} type="button"><span className="font-semibold">使用设备邀请或可信 Node 连接卡</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
          <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
            <p className="text-sm text-muted-foreground">在新设备上扫描自己设备显示的二维码。首次创建账户时，在 Node 终端运行 <code>vibeface access</code>，使用其输出的可信连接卡。</p>
            <form className="space-y-4" onSubmit={event => {
              event.preventDefault(); const form = event.currentTarget
              void run(async () => {
                const password = value(form, 'password'); const confirmation = value(form, 'confirmation')
                try { await device.useConnectionCard(value(form, 'invitation'), password || confirmation ? passphrase(password, confirmation) : undefined) }
                finally { form.reset() }
              })
            }}><fieldset disabled={busy || !usable || !!device.review || !!device.incomingEnrollment} className="space-y-4">
              <label className="grid gap-2 text-sm">设备邀请或连接卡<Textarea key={device.initialInvitation} className="min-h-28 font-mono text-xs" name="invitation" required defaultValue={device.initialInvitation} autoComplete="off" spellCheck={false} /></label>
              <details className="group/passphrase">
                <summary className="flex min-h-11 cursor-pointer list-none items-center justify-between gap-2 rounded-lg text-sm font-medium focus-visible:outline-2 focus-visible:outline-ring [&::-webkit-details-marker]:hidden">首次创建账户时设置恢复包口令<ChevronDown className="size-4 transition-transform group-open/passphrase:rotate-180" /></summary>
                <div className="space-y-4 pt-2">
                  <p className="text-xs text-muted-foreground">至少 12 个字符。请独立保存口令，网站无法找回。</p>
                  <label className="grid gap-2 text-sm">加密恢复包口令<Input className={input} type="password" name="password" autoComplete="new-password" minLength={12} /></label>
                  <label className="grid gap-2 text-sm">再次填写口令<Input className={input} type="password" name="confirmation" autoComplete="new-password" minLength={12} /></label>
                </div>
              </details>
              <Button className={button} type="submit">读取邀请并核对授权</Button>
            </fieldset></form>
          </CollapsibleContent>
        </Collapsible>
      </Card>
      {usable && <RecoveryControls />}
      {device.identity && <Collapsible className="px-1">
        <CollapsibleTrigger asChild><Button variant="ghost" className="min-h-10 w-full justify-between text-muted-foreground [&[data-state=open]>svg]:rotate-180" type="button">此浏览器的设备指纹<ChevronDown className="size-4 transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="px-3 pb-3 data-[state=closed]:hidden"><p className="break-all font-mono text-xs text-muted-foreground">{device.identity.nodeId}</p></CollapsibleContent>
      </Collapsible>}
    </div>
  </main>
}
