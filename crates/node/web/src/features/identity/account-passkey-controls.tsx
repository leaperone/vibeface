import { useEffect, useRef, useState } from 'react'
import { messageOf } from '@/lib/node'
import { useDevice } from './context'

const button = 'min-h-11 rounded-lg border border-border px-3 py-2 text-sm font-medium hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50'
const input = 'min-h-11 w-full rounded-lg border border-input bg-background px-3 py-2 text-base'
type Feedback = { kind: 'success' | 'error' | 'notice'; message: string }

export function AccountPasskeyControls() {
  const device = useDevice()
  const { ready, identity, firstPasskeyPending } = device
  const form = useRef<HTMLFormElement>(null)
  const details = useRef<HTMLDetailsElement>(null)
  const pending = useRef<{ cancelled: boolean } | null>(null)
  const [busy, setBusy] = useState(false)
  const [feedback, setFeedback] = useState<Feedback | null>(null)

  useEffect(() => {
    if (firstPasskeyPending && details.current) details.current.open = true
  }, [firstPasskeyPending])

  useEffect(() => {
    form.current?.reset()
    setFeedback(null)
  }, [ready, identity?.profileId])

  if (!ready?.authorization.enrollmentRights.includes('manage_authenticators')) return null

  async function run(action: () => Promise<void>) {
    if (pending.current || !ready?.isCurrent()) return
    const operation = { cancelled: false }
    pending.current = operation
    setBusy(true)
    setFeedback(null)
    try {
      await action()
      if (!operation.cancelled && ready.isCurrent()) setFeedback({ kind: 'success', message: '账户 Passkey 已登记。独立设备口令仍然有效。' })
    } catch (cause) {
      if (!operation.cancelled && ready.isCurrent()) setFeedback({ kind: 'error', message: messageOf(cause) })
    } finally {
      pending.current = null
      setBusy(false)
    }
  }

  function cancel() {
    if (pending.current) pending.current.cancelled = true
    device.cancelSensitive()
    form.current?.reset()
    setFeedback({ kind: 'notice', message: '已停止本页登记。已提交的变化不会撤回，请重新连接核对结果。' })
  }

  return <details ref={details} className="space-y-4 rounded-xl border border-border bg-card p-4">
    <summary className="min-h-11 cursor-pointer py-2 font-semibold">登记第一个账户 Passkey</summary>
    <div className="space-y-4">
      <p className="text-sm text-muted-foreground">选择当前账户最新的完整加密账户恢复包，并填写恢复包口令。随后按系统提示验证你的身份。登记成功后，独立设备口令仍然有效。</p>
      {feedback && <p role={feedback.kind === 'error' ? 'alert' : 'status'} className={feedback.kind === 'error' ? 'text-sm text-destructive' : 'text-sm text-muted-foreground'}>{feedback.message}</p>}
      {busy && <p role="status" className="text-sm text-muted-foreground">正在处理账户 Passkey，请等待 Node 确认。</p>}
      {firstPasskeyPending && <div role="alert" className="space-y-3 rounded-lg border border-border p-3 text-sm">
        <p>原注册已提交，结果尚待核对。超时或断线后先检查原注册结果，不会自动重新登记 Passkey。</p>
        <button type="button" className={button} disabled={busy} onClick={() => void run(device.resumePasskeyRegistration)}>检查原注册结果</button>
      </div>}
      <form ref={form} className="space-y-3" onSubmit={event => {
        event.preventDefault()
        if (firstPasskeyPending) return
        void run(async () => {
          const element = form.current
          if (!element) return
          const password = element.elements.namedItem('password')
          try {
            const bundle = element.elements.namedItem('bundle')
            const protect = element.elements.namedItem('protect_local')
            const file = bundle instanceof HTMLInputElement ? bundle.files?.[0] : null
            if (!file) throw new Error('请选择当前账户最新的完整账户恢复包。')
            if (!(password instanceof HTMLInputElement) || !password.value) throw new Error('请填写恢复包口令。')
            await device.registerAccountPasskey({ file, password: password.value, protectLocal: !!identity?.profileId && protect instanceof HTMLInputElement && protect.checked })
          } finally {
            if (password instanceof HTMLInputElement) password.value = ''
          }
        })
      }}>
        <fieldset disabled={busy || firstPasskeyPending} className="space-y-3">
          <label className="grid gap-1 text-sm">最新完整账户恢复包<input className={input} type="file" name="bundle" accept="application/json,.json" required /></label>
          <label className="grid gap-1 text-sm">恢复包口令<input className={input} type="password" name="password" autoComplete="current-password" required /></label>
          <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="protect_local" defaultChecked={false} disabled={!identity?.profileId} aria-describedby="account-passkey-local-help" />同时启用本机 Passkey 解锁</label>
          <p id="account-passkey-local-help" className="text-xs text-muted-foreground">{identity?.profileId ? '本机解锁取决于浏览器支持，独立设备口令仍会保留。' : '本机 Passkey 解锁需要已设置独立口令的设备配置，临时会话无法启用。'}</p>
          <button type="submit" className={button}>登记账户 Passkey</button>
        </fieldset>
      </form>
      {(busy || firstPasskeyPending) && <div className="space-y-2">
        <button type="button" className={button} onClick={cancel}>取消并保留已提交结果待核对</button>
        <p className="text-xs text-muted-foreground">取消只停止本页操作，已提交的变化需重新连接核对。</p>
      </div>}
    </div>
  </details>
}
