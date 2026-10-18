import { useEffect, useRef, useState } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Textarea } from '@/components/ui/textarea'
import { downloadGatewayDocument, gatewayActions, gatewayRequestFromCard, parseGatewayRouteRequest } from '@/lib/gateway-owner'
import type { GatewayOwnerInput, GatewayRouteRequest } from '@/lib/gateway-owner'
import { messageOf, record } from '@/lib/node'
import { parseGatewayConnectionCard } from '@/lib/connection-file'
import { readInvitation } from '../../../transport.js'
import { nodeFingerprint } from '../../../generated/vibeface_secure.js'
import { useDevice } from './context'

export function GatewayControls() {
  const device = useDevice(), ready = device.ready, ownerRoute = device.ownerGatewayRoute
  const [mode, setMode] = useState<'approve' | 'revoke'>('approve')
  const [content, setContent] = useState('')
  const [confirmed, setConfirmed] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  const running = useRef(false)
  useEffect(() => { setConfirmed(false) }, [ready?.audiencePublicKey, ready?.project.id])
  if (!ready) return null
  let preview: GatewayRouteRequest | null = null, revoked = false, registeredExpired = false, previewError = ''
  try {
    if (content.trim()) preview = mode === 'approve' ? parseGatewayRouteRequest(readInvitation(content)) : gatewayRequestFromCard(parseGatewayConnectionCard(readInvitation(content)))
    if (ownerRoute) {
      const payload = record(parseGatewayConnectionCard(JSON.parse(ownerRoute.card)).approval.payload)
      revoked = payload.state === 'revoked'; registeredExpired = typeof payload.expires_at_ms === 'number' && payload.expires_at_ms <= Date.now()
    }
  } catch (cause) { previewError = messageOf(cause) }
  const disabled = busy || !!device.review || !!device.nodeEnrollmentReview || !!device.teamCreationReview || !!device.governancePending || device.firstPasskeyPending
  async function run(action: () => Promise<void>, success: string) {
    if (running.current) return
    running.current = true; setBusy(true); setError(''); setStatus('')
    try { await action(); setStatus(success) }
    catch (cause) { setError(messageOf(cause)) }
    finally { running.current = false; setBusy(false) }
  }
  return <Card className="gap-0 py-0">
    <Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="h-auto min-h-14 w-full justify-between gap-3 px-5 py-4 text-left [&[data-state=open]>svg]:rotate-180"><span className="font-semibold">申请或管理业务网关</span><ChevronDown className="size-4 shrink-0 transition-transform" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="space-y-5 px-5 pb-5 data-[state=closed]:hidden">
        <section className="space-y-3" aria-labelledby="gateway-request-title">
          <h3 id="gateway-request-title" className="text-sm font-medium">申请网关转发当前项目</h3>
          <p className="text-xs text-muted-foreground">导出当前浏览器、项目、允许用途和来源根指纹，交给网关所有者批准。申请不含密钥或业务内容。</p>
          <p className="break-all font-mono text-xs text-muted-foreground">{ready.project.id}</p>
          <Button type="button" variant="outline" disabled={disabled} onClick={() => void run(async () => {
            const content = await device.exportGatewayRequest()
            downloadGatewayDocument(content, `vibeface-gateway-request-${ready.project.id.slice(-12)}`)
          }, '公开网关申请已下载。')}>下载公开网关申请</Button>
        </section>
        {!ready.authorization.isTeam && <section className="space-y-4 border-t border-border pt-4" aria-labelledby="gateway-owner-title">
          <div className="space-y-2">
            <h3 id="gateway-owner-title" className="text-sm font-medium">作为当前网关所有者审批</h3>
            <p className="text-xs text-muted-foreground">核对申请人和项目后，使用此账户的恢复包在本机签名。恢复包和口令不会发送给 Node。</p>
            <p className="break-all text-xs">当前网关 {ready.entry.address}</p>
            <p className="break-all font-mono text-xs text-muted-foreground">{nodeFingerprint(ready.audiencePublicKey)}</p>
          </div>
          {ownerRoute && <div className="space-y-3 rounded-lg bg-muted/50 p-3 text-sm">
            <p role="status">{ownerRoute.status === 'pending' ? '原路由结果待核对。继续会使用同一份签名，不创建新路由。' : ownerRoute.status === 'expired' ? '网关已确认原签名过期且没有安装。原记录已保留，可以重新申请。' : ownerRoute.status === 'superseded' ? '网关已有同版本或更新的其他路由。原记录已保留，可以处理新的明确申请。' : revoked ? '撤销签名已登记。原连接卡不能继续转发。' : registeredExpired ? '原路由签名已登记，但签名已过期。原记录已保留，可以重新申请。' : '路由签名已登记。可将连接卡交给申请人；连接时仍会核对权限和有效期。'}</p>
            <div className="flex flex-wrap gap-2">
              {ownerRoute.status === 'pending' && <Button type="button" variant="outline" disabled={disabled} onClick={() => void run(device.retryGatewayRoute, '原路由结果已核对。')}>核对原路由</Button>}
              {ownerRoute.status !== 'pending' && <Button type="button" variant="outline" disabled={disabled} onClick={() => downloadGatewayDocument(ownerRoute.card, ownerRoute.status === 'installed' && !revoked && !registeredExpired ? 'vibeface-gateway-card' : 'vibeface-gateway-route-record')}>{ownerRoute.status === 'installed' && !revoked && !registeredExpired ? '下载已登记连接卡' : '下载原路由记录'}</Button>}
            </div>
          </div>}
          <form className="space-y-4" onSubmit={event => {
            event.preventDefault()
            const form = event.currentTarget
            if (!form.reportValidity()) return
            const data = new FormData(form), file = data.get('bundle')
            if (!(file instanceof File)) return
            const common = { file, password: String(data.get('password') ?? ''), expiresAt: Date.parse(String(data.get('expires_at') ?? '')) }
            const input: GatewayOwnerInput = mode === 'approve' ? { ...common, kind: 'approve', request: content } : { ...common, kind: 'revoke', card: content }
            common.password = ''; data.delete('password')
            const passwordField = form.elements.namedItem('password')
            if (passwordField instanceof HTMLInputElement) passwordField.value = ''
            void run(async () => { try { await device.approveGatewayRoute(input) } finally { input.password = ''; setConfirmed(false); form.reset() } }, '路由处理结果已更新，请查看上方状态。')
          }}>
            <fieldset className="space-y-3" disabled={disabled || ownerRoute?.status === 'pending'}>
              <label className="grid gap-2 text-sm">操作<NativeSelect value={mode} onChange={event => { setMode(event.target.value === 'revoke' ? 'revoke' : 'approve'); setContent(''); setConfirmed(false) }}><NativeSelectOption value="approve">批准公开申请</NativeSelectOption><NativeSelectOption value="revoke">撤销原连接卡</NativeSelectOption></NativeSelect></label>
              <label className="grid gap-2 text-sm">从公开 JSON 文件导入<Input type="file" accept="application/json,.json" onChange={event => {
                setConfirmed(false)
                const file = event.target.files?.[0]
                if (!file) return
                void run(async () => {
                  if (!file.size || file.size > 65536) throw new Error('选择不超过 64 KiB 的公开申请或连接卡。')
                  const value = await file.text()
                  if (!ready.isCurrent()) throw new Error('当前项目已切换，请在当前连接重新导入。')
                  if (mode === 'approve') parseGatewayRouteRequest(readInvitation(value))
                  else gatewayRequestFromCard(parseGatewayConnectionCard(readInvitation(value)))
                  setContent(value); setConfirmed(false)
                }, '公开文件已导入，请核对下面的申请信息。')
              }} /></label>
              <label className="grid gap-2 text-sm">{mode === 'approve' ? '公开网关申请' : '原网关连接卡'}<Textarea rows={5} required autoComplete="off" autoCapitalize="none" spellCheck={false} value={content} onChange={event => { setContent(event.target.value); setConfirmed(false) }} /></label>
              {previewError && <p role="alert" className="text-sm text-destructive">{previewError}</p>}
              {preview && <div className="space-y-3 rounded-lg border border-border p-3 text-xs">
                <dl className="grid gap-3">
                  <div><dt className="text-muted-foreground">申请人设备指纹</dt><dd className="break-all font-mono">{nodeFingerprint(preview.client_key)}</dd></div>
                  <div><dt className="text-muted-foreground">申请人账户</dt><dd className="break-all font-mono">{preview.client_account}</dd></div>
                  <div><dt className="text-muted-foreground">目标 Node 指纹</dt><dd className="break-all font-mono">{nodeFingerprint(preview.target_key)}</dd></div>
                  <div><dt className="text-muted-foreground">项目</dt><dd className="break-all font-mono">{preview.resource.space} / {preview.resource.resource}</dd></div>
                  <div><dt className="text-muted-foreground">允许用途</dt><dd>{preview.allowed_actions.map(action => gatewayActions[action]).join('、')}</dd></div>
                </dl>
                <details><summary className="cursor-pointer py-2 text-muted-foreground">来源根公钥（与申请人核对）</summary><ul className="space-y-2 pt-2">{preview.source_pins.map(pin => <li key={`${pin.domain.kind}:${pin.domain.id}`} className="space-y-1"><p className="break-all font-mono">{pin.domain.id}</p><p className="break-all font-mono text-muted-foreground">{pin.genesis_root}</p></li>)}</ul></details>
              </div>}
              <label className="grid gap-2 text-sm">完整账户恢复包<Input name="bundle" type="file" accept="application/json,.json" required onChange={() => setConfirmed(false)} /></label>
              <label className="grid gap-2 text-sm">恢复包口令<Input name="password" type="password" autoComplete="off" required /></label>
              <label className="grid gap-2 text-sm">本次签名有效期至<Input name="expires_at" type="datetime-local" required onChange={() => setConfirmed(false)} /></label>
              <label className="flex min-h-11 items-center gap-2 text-sm"><input name="confirmed" type="checkbox" required checked={confirmed} onChange={event => setConfirmed(event.target.checked)} />{mode === 'approve' ? '我已核对申请人、项目和允许用途' : '我确认撤销这份原连接卡'}</label>
              <Button type="submit" disabled={!preview}>{mode === 'approve' ? '签名并安装路由' : '签名并撤销路由'}</Button>
            </fieldset>
          </form>
        </section>}
        {(busy || status) && <p role="status" className="text-sm text-muted-foreground">{busy ? '正在本地签名并核对路由…' : status}</p>}
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      </CollapsibleContent>
    </Collapsible>
  </Card>
}
