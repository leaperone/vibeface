import { useRef, useState } from 'react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Label } from '@/components/ui/label'
import { messageOf } from '@/lib/node'
import { passphrase } from '@/lib/secure'
import { useDevice } from './context'

export function VaultControls() {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const pending = useRef(false)
  const [creating, setCreating] = useState(false)
  const locked = device.phase === 'vault_locked' && !creating
  const lost = device.phase === 'key_lost'
  const unavailable = device.phase === 'vault_unavailable'
  async function run(action: () => Promise<void>) {
    if (pending.current) return
    pending.current = true; setBusy(true); setError('')
    try { await action() } catch (cause) { setError(messageOf(cause)) }
    finally { pending.current = false; setBusy(false) }
  }
  return <section className="space-y-4 rounded-xl border border-border bg-card p-5" aria-labelledby="vault-title">
    <h2 id="vault-title" className="font-semibold">{locked ? '解锁设备仓库' : unavailable ? '加密设备仓库暂不可用' : lost ? '设备仓库已丢失' : '创建账户的独立设备配置'}</h2>
    <p className="text-sm text-muted-foreground">{locked ? '口令仅在此页面解密设备密钥，不会发送给节点。锁定会结束连接，原操作仍需在原空间核对。' : '使用至少 12 个字符的本地口令加密保存设备密钥。请独立保存口令，无法通过网站找回。旧版明文仓库不会导入。'}</p>
    {lost && <p className="text-sm">请恢复加密设备备份，或创建新设备并重新取得批准。原设备的授权不会自动转移。</p>}
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    <form className="space-y-3" onSubmit={event => {
      event.preventDefault()
      const form = event.currentTarget
      void run(async () => {
        const data = new FormData(form)
        const password = data.get('password')
        const confirmation = data.get('confirmation')
        if (typeof password !== 'string') throw new Error('请输入仓库口令。')
        try {
          const profileId = data.get('profile')
          const name = data.get('name')
          if (locked && typeof profileId !== 'string') throw new Error('请选择设备配置。')
          await device.initializeVault(locked && typeof profileId === 'string' ? { mode: 'unlock', profileId, password } : { mode: 'create', name: typeof name === 'string' ? name : '', password: passphrase(password, typeof confirmation === 'string' ? confirmation : '') })
        } finally { form.reset() }
      })
    }}><fieldset disabled={busy || unavailable} className="space-y-3">
      {locked ? <div className="space-y-1"><Label htmlFor="vault-profile">设备配置</Label><NativeSelect id="vault-profile" name="profile" required>{device.vaults.map(profile => <NativeSelectOption key={profile.id} value={profile.id}>{profile.name}</NativeSelectOption>)}</NativeSelect></div> : <div className="space-y-1"><Label htmlFor="vault-name">设备配置名称</Label><Input id="vault-name" name="name" required maxLength={128} placeholder="例如：个人账户的手机" autoComplete="off" /></div>}
      <div className="space-y-1"><Label htmlFor="vault-password">设备仓库口令</Label><Input id="vault-password" name="password" type="password" autoComplete={locked ? 'current-password' : 'new-password'} required minLength={locked ? undefined : 12} /></div>
      {!locked && <div className="space-y-1"><Label htmlFor="vault-confirmation">再次填写口令</Label><Input id="vault-confirmation" name="confirmation" type="password" autoComplete="new-password" required minLength={12} /></div>}
      <Button type="submit">{busy ? '正在处理…' : locked ? '解锁设备仓库' : '创建加密设备仓库'}</Button>
    </fieldset></form>
    {device.vaults.length > 0 && <Button type="button" variant="ghost" disabled={busy} onClick={() => { setCreating(!creating); setError('') }}>{creating ? '返回已有设备配置' : '为其他账户创建独立设备配置'}</Button>}
    <details className="space-y-3">
      <summary className="cursor-pointer py-2 text-sm">恢复此浏览器的加密设备备份</summary>
      <form className="space-y-3" onSubmit={event => {
        event.preventDefault()
        const form = event.currentTarget
        void run(async () => {
          const data = new FormData(form)
          const file = data.get('vault')
          const password = data.get('password')
          if (!(file instanceof File) || !file.size || file.size > 16384) throw new Error('选择不超过 16 KiB 的加密设备备份。')
          if (typeof password !== 'string') throw new Error('请输入备份口令。')
          try {
            const vault: unknown = JSON.parse(await file.text())
            await device.initializeVault({ mode: 'restore', vault, password })
          } finally { form.reset() }
        })
      }}><fieldset disabled={busy || unavailable} className="space-y-3">
        <div className="space-y-1"><Label htmlFor="vault-file">加密设备备份</Label><Input id="vault-file" type="file" name="vault" accept="application/json,.json" required /></div>
        <div className="space-y-1"><Label htmlFor="vault-restore-password">备份口令</Label><Input id="vault-restore-password" type="password" name="password" autoComplete="current-password" required /></div>
        <Button type="submit" variant="outline">恢复加密设备仓库</Button>
      </fieldset></form>
    </details>
    <div className="space-y-2 border-t border-border pt-3">
      <p className="text-sm text-muted-foreground">临时会话仅在当前页面保留新设备密钥。关闭或锁定后无法恢复，需要重新批准。已有仓库保持不变。</p>
      <Button type="button" variant="outline" disabled={busy} onClick={() => void run(() => device.initializeVault({ mode: 'temporary' }))}>仅使用临时会话</Button>
    </div>
    {busy && <Button type="button" variant="ghost" onClick={device.lockVault}>取消并锁定</Button>}
  </section>
}
