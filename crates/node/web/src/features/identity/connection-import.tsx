import { useEffect, useRef, useState } from 'react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { messageOf } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { inspectConnectionPackage, readConnectionFile } from '@/lib/connection-file'
import type { ConnectionPackage } from '@/lib/connection-file'
import { useDevice } from './context'

export function ConnectionImport({ disabled }: { disabled: boolean }) {
  const device = useDevice()
  const [content, setContent] = useState(device.initialInvitation)
  const [preview, setPreview] = useState<ConnectionPackage | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const generation = useRef(0)
  const reading = useRef<AbortController | null>(null)
  useEffect(() => { generation.current += 1; reading.current?.abort(); setBusy(false); setContent(device.initialInvitation); setPreview(null) }, [device.initialInvitation])
  useEffect(() => () => { generation.current += 1; reading.current?.abort() }, [])
  function change(value: string) { generation.current += 1; reading.current?.abort(); setBusy(false); setContent(value); setPreview(null); setError('') }
  async function inspect(value: string | File) {
    const version = ++generation.current
    reading.current?.abort()
    const controller = new AbortController()
    reading.current = controller
    setBusy(true); setError(''); setPreview(null)
    try {
      const text = typeof value === 'string' ? value : await readConnectionFile(value)
      controller.signal.throwIfAborted()
      if (generation.current === version) setContent(text)
      const result = await inspectConnectionPackage(text, controller.signal)
      if (generation.current === version) setPreview(result)
    } catch (cause) { if (generation.current === version) setError(messageOf(cause)) }
    finally { if (generation.current === version) setBusy(false) }
  }
  return <div className="space-y-4">
    <p className="text-sm text-muted-foreground">选择自己设备导出的文件，或粘贴邀请。先核对目标和权限，再确认接入。</p>
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    <form className="space-y-4" onSubmit={event => { event.preventDefault(); void inspect(content) }}>
      <fieldset disabled={disabled || busy} className="space-y-4">
        <label className="grid gap-2 text-sm">连接或邀请文件<Input className="min-h-11 h-auto py-2" type="file" accept="application/json,.json" onChange={event => {
          const file = event.target.files?.[0]
          event.target.value = ''
          if (!file) return
          void inspect(file)
        }} /></label>
        <label className="grid gap-2 text-sm">连接卡或邀请内容<Textarea className="min-h-28 font-mono text-xs" required value={content} onChange={event => change(event.target.value)} autoComplete="off" spellCheck={false} /></label>
        <Button className="min-h-11" type="submit">{busy ? '正在核对…' : '读取并核对'}</Button>
      </fieldset>
    </form>
    {preview && <form className="space-y-4 rounded-lg bg-muted/50 p-4" onSubmit={event => {
      event.preventDefault()
      const form = event.currentTarget
      const password = form.elements.namedItem('password'), confirmation = form.elements.namedItem('confirmation')
      void (async () => {
        setBusy(true); setError('')
        try {
          const secret = preview.kind === 'claim' && password instanceof HTMLInputElement && confirmation instanceof HTMLInputElement ? passphrase(password.value, confirmation.value) : undefined
          await device.useConnectionCard(preview.content, secret)
          setPreview(null); setContent('')
        } catch (cause) { setError(messageOf(cause)) }
        finally { form.reset(); setBusy(false) }
      })()
    }}>
      <h3 className="font-medium">{preview.kind === 'claim' ? '首次认领节点' : preview.kind === 'enrollment' ? '新设备入网邀请' : '连接已授权节点'}</h3>
      <dl className="grid gap-3 text-sm">
        <div><dt className="text-muted-foreground">连接路径</dt><dd className="break-all">{preview.entry.address} · {preview.entry.relay ? '经中继' : '直连'}</dd></div>
        <div><dt className="text-muted-foreground">目标节点指纹</dt><dd className="break-all font-mono text-xs">{preview.entry.node_id}</dd></div>
        <div><dt className="text-muted-foreground">目标节点公钥</dt><dd className="break-all font-mono text-xs">{preview.nodePublicKey}</dd></div>
        {preview.kind === 'enrollment' && <>
          <div><dt className="text-muted-foreground">账户</dt><dd className="break-all font-mono text-xs">{preview.accountId}</dd></div>
          <div><dt className="text-muted-foreground">新增权限</dt><dd>{preview.rights.length ? preview.rights.map(right => right === 'enroll_device' ? '添加设备' : right === 'revoke_device' ? '撤销设备' : right).join('、') : '普通客户端，无设备管理权限'}</dd></div>
          <div><dt className="text-muted-foreground">邀请有效期</dt><dd>{new Date(preview.expiresAt).toLocaleString()}</dd></div>
        </>}
      </dl>
      {preview.kind === 'claim' && <fieldset disabled={busy} className="space-y-3">
        <p className="text-sm text-muted-foreground">将创建新的个人账户。请设置恢复包口令，并在后续步骤保存恢复包。</p>
        <label className="grid gap-2 text-sm">恢复包口令<Input className="min-h-11" name="password" type="password" minLength={12} autoComplete="new-password" required /></label>
        <label className="grid gap-2 text-sm">再次填写口令<Input className="min-h-11" name="confirmation" type="password" minLength={12} autoComplete="new-password" required /></label>
      </fieldset>}
      <p className="text-xs text-muted-foreground">{preview.kind === 'enrollment' ? '确认后将提交入网申请，等待管理设备核对。批准后还需由此设备提供持钥证明。' : preview.kind === 'connection' ? '连接卡不授予新权限，将使用此设备已有的授权。' : '仅用于尚未认领的节点。'}</p>
      <div className="flex flex-wrap gap-2"><Button className="min-h-11" type="submit" disabled={busy || disabled}>确认{preview.kind === 'enrollment' ? '申请加入' : preview.kind === 'claim' ? '认领' : '连接'}</Button><Button className="min-h-11" type="button" variant="outline" disabled={busy} onClick={() => setPreview(null)}>取消</Button></div>
    </form>}
  </div>
}
