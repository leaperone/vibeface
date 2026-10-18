import { useRef, useState } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import type { Member } from '@/lib/node'
import { messageOf, parseEntry } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { useDevice } from './context'

const input = 'min-h-10'
const button = 'min-h-10'
const panel = 'gap-4 p-5'
const disclosure = 'h-auto min-h-14 w-full justify-between gap-3 rounded-none px-5 py-4 text-left whitespace-normal [&[data-state=open]>svg]:rotate-180'
function field(form: HTMLFormElement, name: string) { const element = form.elements.namedItem(name); return element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement ? element.value : '' }
function fileOf(form: HTMLFormElement) { const element = form.elements.namedItem('bundle'); const file = element instanceof HTMLInputElement ? element.files?.[0] : null; if (!file) throw new Error('请选择加密恢复包。'); return file }

export function RecoveryReview() {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const [error, setError] = useState('')
  const formRef = useRef<HTMLFormElement>(null)
  const review = device.review
  if (!review) return null
  async function run(action: () => Promise<void>) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError('')
    try { await action() } catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <Card className={panel} aria-labelledby="recovery-review-title">
    <h2 id="recovery-review-title" className="font-semibold">{review.kind === 'bootstrap' ? '先保存加密恢复包' : '确认恢复后的设备授权'}</h2>
    <p className="break-all font-mono text-xs">个人网络 {review.networkId}</p>
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    <p role="status" className="text-sm text-muted-foreground">{busy ? '正在提交授权，请等待设备确认。' : review.kind === 'bootstrap' ? '创建网络的根私钥只在此浏览器里使用，不会发给节点。' : '恢复根私钥只在此可信客户端使用。未加密私钥不会发送给节点。'}</p>
    {review.kind === 'bootstrap' ? <form className="space-y-4" onSubmit={event => {
      event.preventDefault(); const checkbox = event.currentTarget.elements.namedItem('saved')
      void run(() => device.confirmBootstrap(checkbox instanceof HTMLInputElement && checkbox.checked))
    }}>
      <p className="text-sm text-muted-foreground">下载恢复包，并独立保存口令。保存后才能创建个人网络。</p>
      <Button variant="outline" className={button} type="button" disabled={busy} onClick={() => void run(async () => device.downloadBackup())}>下载加密恢复包</Button>
      <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="saved" required disabled={busy} />我已保存恢复包，并妥善保存口令</label>
      <Button className={button} type="submit" disabled={busy}>确认保存并创建网络</Button>
    </form> : <form ref={formRef} className="space-y-4" onSubmit={event => {
      event.preventDefault(); const form = event.currentTarget
      void run(async () => {
        const selected: Member[] = review.members.filter(member => { const checkbox = form.elements.namedItem(`keep-${member.subject_id}`); return checkbox instanceof HTMLInputElement && checkbox.checked }).map(member => {
          const manage = form.elements.namedItem(`manage-${member.subject_id}`)
          return { ...member, manage: member.kind === 'client' && manage instanceof HTMLInputElement && manage.checked }
        })
        await device.confirmRecovery(selected)
      })
    }}>
      <p className="text-sm text-muted-foreground">非管理成员默认留下，旧管理端默认不选。当前节点必须留下。</p>
      <fieldset disabled={busy} className="space-y-4">{review.members.map(member => <div key={member.subject_id} className="space-y-2 rounded-lg border border-border p-3"><label className="flex min-h-11 items-center gap-2 break-all text-sm"><input type="checkbox" name={`keep-${member.subject_id}`} defaultChecked={!member.manage || member.subject_id === review.requiredNode} disabled={member.subject_id === review.requiredNode} />保留 {member.name}{member.subject_id === review.requiredNode ? '（当前目标，必须保留）' : ''}</label><p className="break-all font-mono text-xs text-muted-foreground">{member.subject_id}</p>{member.kind === 'client' ? <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name={`manage-${member.subject_id}`} />允许此客户端管理设备</label> : <p className="text-xs text-muted-foreground">节点角色 {member.node_roles.join('、')}</p>}</div>)}</fieldset>
      <p className="break-all text-sm">此浏览器「{review.newManager.name}」将成为管理客户端。</p><p className="break-all font-mono text-xs text-muted-foreground">{review.newManager.subject_id}</p>
      <Button className={button} type="submit" disabled={busy}>签名确认并恢复管理权限</Button>
      <p className="text-xs text-muted-foreground">成功后将下载更新授权版本的加密恢复包，请保存新文件。</p>
    </form>}
    <Button variant="outline" className={`${button} self-start`} type="button" onClick={device.cancelSensitive}>{busy ? '取消等待并清除解锁材料' : '取消并清除解锁材料'}</Button>
    {busy && <p className="text-xs text-muted-foreground">若授权已提交，取消不会撤回提交。重新连接后请核对当前授权。</p>}
  </Card>
}

export function RecoveryControls({ only }: { only?: 'restore' | 'passphrase' }) {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  async function run(action: () => Promise<void>, success = '') {
    if (pending.current) return
    pending.current = true; setBusy(true); setError(''); setStatus('')
    try { await action(); setStatus(success) } catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <div className="space-y-4">
    {device.ready && device.review && <RecoveryReview />}
    {error && <p role="alert" className="rounded-lg border border-destructive p-3 text-sm text-destructive">{error}</p>}
    {(busy || status) && <p role="status" className="text-sm text-muted-foreground">{busy ? '正在验证恢复包及目标设备…' : status}</p>}
    {only !== 'passphrase' && <Card className="gap-0 py-0">
      <Collapsible defaultOpen={only === 'restore'}>
        <CollapsibleTrigger asChild><Button variant="ghost" className={disclosure} type="button"><span className="font-semibold">恢复管理权限</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
          <p className="text-sm text-muted-foreground">恢复根私钥不会上传节点。恢复只重建授权，聊天正文和工程文件仍需可用副本。</p>
          <form className="space-y-4" onSubmit={event => {
            event.preventDefault(); const form = event.currentTarget
            void run(async () => {
              try {
                const relay = form.elements.namedItem('relay')
                await device.beginRecovery({ file: fileOf(form), password: field(form, 'password'), name: field(form, 'name'), bootstrap: field(form, 'bootstrap'), entry: parseEntry({ address: field(form, 'address'), node_id: field(form, 'node_id'), relay: relay instanceof HTMLInputElement && relay.checked }) })
              } finally { form.reset() }
            })
          }}><fieldset disabled={busy || !!device.review} className="space-y-4">
            <label className="grid gap-2 text-sm">加密恢复包<Input className={`${input} h-auto py-2`} type="file" name="bundle" accept="application/json,.json" required /></label>
            <label className="grid gap-2 text-sm">恢复包口令<Input className={input} type="password" name="password" autoComplete="off" required /></label>
            <label className="grid gap-2 text-sm">此浏览器名称<Input className={input} name="name" required maxLength={128} /></label>
            <label className="grid gap-2 text-sm">目标入口地址<Input className={input} type="url" name="address" required defaultValue={device.ready?.entry.address || device.entry?.address || ''} /></label>
            <label className="grid gap-2 text-sm">目标节点完整指纹<Input className={`${input} font-mono`} name="node_id" required pattern="[a-f0-9]{64}" defaultValue={device.ready?.entry.node_id || device.entry?.node_id || ''} autoComplete="off" spellCheck={false} /></label>
            <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="relay" defaultChecked={device.ready?.entry.relay || device.entry?.relay || false} />通过入口中继到目标节点</label>
            <label className="grid gap-2 text-sm">新节点的本机初始化邀请，可选<Textarea className="min-h-20 font-mono text-xs" name="bootstrap" autoComplete="off" spellCheck={false} /></label>
            <Button className={button} type="submit">解锁并核对恢复授权</Button>
          </fieldset></form>
        </CollapsibleContent>
      </Collapsible>
    </Card>}
    {only !== 'restore' && <Card className="gap-0 py-0">
      <Collapsible defaultOpen={only === 'passphrase'}>
        <CollapsibleTrigger asChild><Button variant="ghost" className={disclosure} type="button"><span className="font-semibold">更换恢复包口令</span><ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform" /></Button></CollapsibleTrigger>
        <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
          <p className="text-sm text-muted-foreground">生成使用新口令的文件。原文件仍可用原口令解锁，请分别保管。</p>
          <form className="space-y-4" onSubmit={event => {
            event.preventDefault(); const form = event.currentTarget
            void run(async () => {
              try { await device.reencrypt({ file: fileOf(form), oldPassword: field(form, 'old_password'), password: passphrase(field(form, 'password'), field(form, 'confirmation')) }) }
              finally { form.reset() }
            }, '新的加密恢复包已下载。')
          }}><fieldset disabled={busy || !!device.review} className="space-y-4">
            <label className="grid gap-2 text-sm">加密恢复包<Input className={`${input} h-auto py-2`} type="file" name="bundle" accept="application/json,.json" required /></label>
            <label className="grid gap-2 text-sm">原口令<Input className={input} type="password" name="old_password" autoComplete="off" required /></label>
            <label className="grid gap-2 text-sm">新口令，至少 12 个字符<Input className={input} type="password" name="password" minLength={12} autoComplete="new-password" required /></label>
            <label className="grid gap-2 text-sm">再次填写新口令<Input className={input} type="password" name="confirmation" minLength={12} autoComplete="new-password" required /></label>
            <Button className={button} type="submit">下载使用新口令的恢复包</Button>
          </fieldset></form>
        </CollapsibleContent>
      </Collapsible>
    </Card>}
  </div>
}
