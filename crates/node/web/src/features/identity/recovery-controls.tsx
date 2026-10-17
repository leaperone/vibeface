import { useRef, useState } from 'react'
import { messageOf } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { useDevice } from './context'

const input = 'min-h-11 w-full rounded-lg border border-input bg-background px-3 py-2 text-base'
const button = 'min-h-11 rounded-lg border border-border px-4 py-2 text-sm font-medium hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50'
const panel = 'space-y-4 rounded-xl border border-border bg-card p-5'
function field(form: HTMLFormElement, name: string) { const element = form.elements.namedItem(name); return element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement ? element.value : '' }
function fileOf(form: HTMLFormElement) { const element = form.elements.namedItem('bundle'); const file = element instanceof HTMLInputElement ? element.files?.[0] : null; if (!file) throw new Error('请选择完整账户恢复包。'); return file }

export function RecoveryReview() {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const pending = useRef(false)
  const [error, setError] = useState('')
  const review = device.review
  if (!review) return null
  async function run(action: () => Promise<void>) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError('')
    try { await action() } catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <section className={panel} aria-labelledby="recovery-review-title">
    <h2 id="recovery-review-title" className="font-semibold">{review.kind === 'bootstrap' ? '保存恢复包并创建账户' : '保存更新恢复包并恢复账户'}</h2>
    <dl className="space-y-2 text-xs"><div><dt>账户</dt><dd className="break-all font-mono">{review.accountId}</dd></div><div><dt>个人空间</dt><dd className="break-all font-mono">{review.spaceId}</dd></div></dl>
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    <p role="status" className="text-sm text-muted-foreground">{busy ? '正在提交，请等待 Node 确认。' : '账户根与恢复密钥只在此可信页面内使用，私钥不会发送给 Node。'}</p>
    {review.kind === 'recovery' && <div className="space-y-2 text-sm"><p>恢复会更换账户根和客户端密钥，并撤销旧客户端、旧 Passkey 和旧授权。离线 Node 需要同步新政策后才会拒绝旧权限。</p><p>{review.targetChanged ? '本次恢复到另一台 Node。原项目仍保留原来的归属，新 Node 使用独立空项目，聊天和工程文件不会自动迁移。' : '本次仍使用原 Node。业务数据是否完整取决于现有副本。'}</p></div>}
    <div className="space-y-2 rounded-lg border border-border p-3 text-sm"><p>允许这台 Node 在以下项目内运行调度与执行服务。此授权不能转授，也不包含账户或成员管理权限。</p><p className="break-all font-mono text-xs">{review.serviceGrant.projectId}</p><p>允许的操作</p><ul className="list-inside list-disc">{review.serviceGrant.actions.map(action => <li key={action}>{serviceAction(action)}</li>)}</ul></div>
    <form className="space-y-3" onSubmit={event => {
      event.preventDefault(); const checkbox = event.currentTarget.elements.namedItem('saved')
      const saved = checkbox instanceof HTMLInputElement && checkbox.checked
      void run(() => review.kind === 'bootstrap' ? device.confirmBootstrap(saved) : device.confirmRecovery(saved))
    }}>
      <p className="text-sm text-muted-foreground">完整恢复包包含加密的账户根、恢复密钥和可信连接信息。请下载并独立保存口令，确认后才提交。</p>
      <button className={button} type="button" disabled={busy} onClick={() => void run(async () => device.downloadBackup())}>下载完整账户恢复包</button>
      <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="saved" required disabled={busy} />我已保存恢复包，并妥善保存口令</label>
      <button className={button} type="submit" disabled={busy}>{review.kind === 'bootstrap' ? '确认并创建账户' : '确认撤销旧权限并恢复'}</button>
    </form>
    <button className={button} type="button" onClick={device.cancelSensitive}>{busy ? '取消等待并清除敏感材料' : '取消并清除敏感材料'}</button>
    {busy && <p className="text-xs text-muted-foreground">已提交的操作无法通过取消撤回。断线或取消后请先重新连接核对结果。</p>}
  </section>
}

function serviceAction(action: string) {
  const labels: Record<string, string> = {
    conversation_read: '读取项目会话', conversation_create: '创建项目会话', message_append: '添加会话消息',
    task_create: '创建任务', task_accept: '接受任务', task_progress: '更新任务进展', task_cancel: '取消任务',
    execution_open: '启动执行会话', execution_send: '发送执行指令', execution_steer: '补充当前执行指令',
    execution_interrupt: '中断执行', operation_status_read: '读取执行状态', operation_reconcile: '核对执行结果',
    notification_read: '读取任务通知', notification_ack: '确认任务通知',
  }
  return labels[action] ?? action
}

export function RecoveryControls() {
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
  return <div className="space-y-5">
    {device.ready && device.review && <RecoveryReview />}
    <section className={panel} aria-labelledby="restore-title">
      <h2 id="restore-title" className="font-semibold">使用完整账户恢复包</h2>
      <p className="text-sm text-muted-foreground">先为恢复创建新的独立设备配置。入口和根信任从加密包内验证取得，恢复不会迁移聊天和工程文件。</p>
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      <p role="status" className="text-sm text-muted-foreground">{busy ? '正在验证账户包与目标 Node…' : status}</p>
      <form className="space-y-3" onSubmit={event => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          try { await device.beginRecovery({ file: fileOf(form), password: field(form, 'password'), connectionCard: field(form, 'connection_card') }) }
          finally { form.reset() }
        })
      }}><fieldset disabled={busy || !!device.review} className="space-y-3">
        <label className="grid gap-1 text-sm">完整账户恢复包<input className={input} type="file" name="bundle" accept="application/json,.json" required /></label>
        <label className="grid gap-1 text-sm">恢复包口令<input className={input} type="password" name="password" autoComplete="off" required /></label>
        <details><summary className="cursor-pointer py-2 text-sm">更换原 Node 的入口地址</summary><div className="space-y-2 pt-2"><p className="text-sm text-muted-foreground">使用从原 Node 本机或可信渠道取得的连接卡。Node 公钥必须与恢复包内保存的身份相同。</p><label className="grid gap-1 text-sm">原 Node 连接卡<textarea className={`${input} min-h-24 font-mono text-xs`} name="connection_card" autoComplete="off" spellCheck={false} /></label></div></details>
        <button className={button} type="submit">解锁并核对恢复授权</button>
      </fieldset></form>
    </section>
    <section className={panel} aria-labelledby="reencrypt-title">
      <h2 id="reencrypt-title" className="font-semibold">更换恢复包口令</h2>
      <p className="text-sm text-muted-foreground">生成使用新口令的文件。原文件仍可用原口令解锁，请分别保管。</p>
      <form className="space-y-3" onSubmit={event => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          try { await device.reencrypt({ file: fileOf(form), oldPassword: field(form, 'old_password'), password: passphrase(field(form, 'password'), field(form, 'confirmation')) }) }
          finally { form.reset() }
        }, '新的完整账户恢复包已下载。')
      }}><fieldset disabled={busy || !!device.review} className="space-y-3">
        <label className="grid gap-1 text-sm">完整账户恢复包<input className={input} type="file" name="bundle" accept="application/json,.json" required /></label>
        <label className="grid gap-1 text-sm">原口令<input className={input} type="password" name="old_password" autoComplete="off" required /></label>
        <label className="grid gap-1 text-sm">新口令，至少 12 个字符<input className={input} type="password" name="password" minLength={12} autoComplete="new-password" required /></label>
        <label className="grid gap-1 text-sm">再次填写新口令<input className={input} type="password" name="confirmation" minLength={12} autoComplete="new-password" required /></label>
        <button className={button} type="submit">下载使用新口令的恢复包</button>
      </fieldset></form>
    </section>
  </div>
}
