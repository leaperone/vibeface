import { useRef, useState } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { messageOf } from '@/lib/node'
import { useDevice } from './context'

export function NodeControls() {
  const device = useDevice()
  const pending = useRef(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  const review = device.nodeEnrollmentReview
  async function run(action: () => Promise<void>, success = '') {
    if (pending.current) return
    pending.current = true; setBusy(true); setError(''); setStatus('')
    try { await action(); setStatus(success) } catch (error) { setError(messageOf(error)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <Card className="gap-0 py-0">
    <Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="h-auto min-h-14 w-full justify-between px-5 py-4 text-left [&[data-state=open]>svg]:rotate-180"><span className="font-semibold">将另一台 Node 加入自己的账户</span><ChevronDown className="size-4 shrink-0 transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-4 px-5 pb-5 data-[state=closed]:hidden">
        <p className="text-sm text-muted-foreground">保留当前授权 Node 的连接，使用新 Node 本机运行 vibeface access 取得的可信连接卡。账户根只在本页面解密，不发送给任何 Node。</p>
        <p className="text-sm text-muted-foreground">本次只登记 Node 服务设备证书。项目数据、调度和执行权限需要另行明确授权。</p>
        <form className="space-y-4" onSubmit={event => {
          event.preventDefault()
          const form = event.currentTarget
          if (!form.reportValidity()) return
          const data = new FormData(form)
          const file = data.get('bundle')
          if (!(file instanceof File)) return
          const input = { file, password: String(data.get('password') ?? ''), connectionCard: String(data.get('connection_card') ?? ''), certificateExpiresAt: Date.parse(String(data.get('expires_at') ?? '')) }
          void run(async () => { try { await device.beginNodeEnrollment(input) } finally { input.password = ''; form.reset() } })
        }}>
          <fieldset disabled={busy || !!review || !!device.review || device.firstPasskeyPending || !!device.teamCreationReview || !!device.governancePending} className="space-y-3">
            <label className="grid gap-2 text-sm">完整账户恢复包<Input name="bundle" type="file" accept="application/json,.json" required /></label>
            <label className="grid gap-2 text-sm">恢复包口令<Input name="password" type="password" autoComplete="off" required /></label>
            <label className="grid gap-2 text-sm">新 Node 的可信连接卡<Textarea name="connection_card" className="min-h-24 font-mono text-xs" autoComplete="off" spellCheck={false} required /></label>
            <label className="grid gap-2 text-sm">服务设备证书有效期至<Input name="expires_at" type="datetime-local" required /></label>
            <Button type="submit">核对新 Node 持有证明</Button>
          </fieldset>
        </form>
      </CollapsibleContent>
    </Collapsible>
    {review && <div className="space-y-4 border-t p-5">
      <h3 className="text-sm font-semibold">确认新 Node 的完整身份</h3>
      <dl className="grid gap-3 text-xs"><div><dt>账户</dt><dd className="break-all font-mono">{review.accountId}</dd></div><div><dt>新 Node 公钥</dt><dd className="break-all font-mono">{review.nodePublicKey}</dd></div><div><dt>新 Node 地址</dt><dd className="break-all">{review.address}</dd></div></dl>
      <p className="text-sm text-muted-foreground">核对本机终端显示的完整公钥后批准。不会创建新账户，也不会增加业务授权。</p>
      {review.submitted && <p role="status" className="text-sm">原请求可能已提交。继续时会核对同一请求并安装原回执，不生成新设备证书。</p>}
      <Button type="button" disabled={busy} onClick={() => void run(device.confirmNodeEnrollment, 'Node 已加入账户。当前授权 Node 的连接已保留。')}>{review.submitted ? '核对原提交并继续安装' : '批准此 Node 加入账户'}</Button>
      <Button type="button" variant="outline" onClick={device.cancelSensitive}>{busy ? '取消等待并清除材料' : '关闭并清除材料'}</Button>
      {review.submitted && <p className="text-xs text-muted-foreground">取消等待不会撤回已提交的证书。关闭后请先刷新设备目录核对。</p>}
    </div>}
    {(busy || status) && <p role="status" className="px-5 pb-4 text-sm text-muted-foreground">{busy ? '正在核对原 Node 入网请求…' : status}</p>}
    {error && <p role="alert" className="px-5 pb-4 text-sm text-destructive">{error}</p>}
  </Card>
}
