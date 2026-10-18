import { useState } from 'react'
import { Card } from '@/components/ui/card'
import { messageOf } from '@/lib/node'
import { useDevice } from './context'

export function LocalAccessControls() {
  const device = useDevice()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const local = device.localAccess
  if (!local) return null
  return <Card className="gap-3 p-5">
    <label className="flex min-h-11 items-center justify-between gap-4 text-sm font-medium">
      允许本机自动登录
      <input type="checkbox" role="switch" className="size-5 accent-primary" checked={local.enabled} disabled={busy || !local.authenticated} onChange={event => {
        const enabled = event.target.checked
        setBusy(true); setError('')
        void device.setLocalAccess(enabled).catch(cause => setError(messageOf(cause))).finally(() => setBusy(false))
      }} />
    </label>
    <p className="text-sm text-muted-foreground">开启后，此浏览器可自动解锁并连接本机节点。首次使用仍需完成账户登录。公网页面继续验证身份。</p>
    {!local.authenticated && <p className="text-xs text-muted-foreground">在节点本机运行 <code>vibeface open</code>，即可管理此设置和启用自动登录。</p>}
    {local.enabled && <p className="text-xs text-muted-foreground">关闭会锁定本机页面，已启动的任务继续运行。</p>}
    {device.localAccessNotice && <p role="status" className="text-sm text-muted-foreground">{device.localAccessNotice}</p>}
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
  </Card>
}
