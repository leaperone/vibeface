import { useMemo, useState } from 'react'
import { messageOf } from '@/lib/node'
import { createEnrollmentQr } from '@/lib/qr'
import { Button } from '@/components/ui/button'
import { downloadConnectionFile } from '@/lib/connection-file'
import { readInvitation } from '../../../transport.js'

export function EnrollmentQr({ link }: { link: string }) {
  const [status, setStatus] = useState('')
  const [error, setError] = useState('')
  const code = useMemo(() => {
    try { return { kind: 'ready', ...createEnrollmentQr(link) } satisfies { kind: 'ready'; src: string; size: number } }
    catch (error) { return { kind: 'error', error: messageOf(error) } satisfies { kind: 'error'; error: string } }
  }, [link])
  return <div className="space-y-3">
    {code.kind === 'ready' ? <figure className="space-y-2"><img className="h-auto max-w-full rounded-lg border border-border" src={code.src} width={code.size} height={code.size} alt="使用新设备的相机扫描此邀请二维码" style={{ imageRendering: 'pixelated' }} /><figcaption className="text-sm text-muted-foreground">用新设备的相机扫描，再在原设备上核对并批准新设备。</figcaption></figure> : <p role="alert" className="text-sm text-destructive">{code.error}</p>}
    <label className="grid gap-1 text-sm">设备邀请链接<textarea className="min-h-24 w-full rounded-lg border border-input bg-background p-3 font-mono text-xs" readOnly value={link} onFocus={event => event.target.select()} /></label>
    <div className="flex flex-wrap gap-2">
      <Button type="button" className="min-h-11" onClick={() => void navigator.clipboard.writeText(link).then(() => { setStatus('邀请已复制。'); setError('') }).catch(cause => setError(messageOf(cause)))}>复制邀请</Button>
      <Button type="button" variant="outline" className="min-h-11" onClick={() => {
        try { downloadConnectionFile(JSON.stringify(readInvitation(link)), 'device-invitation.vibeface.json'); setStatus('邀请文件已下载。'); setError('') }
        catch (cause) { setError(messageOf(cause)) }
      }}>下载邀请文件</Button>
    </div>
    <p role="status" className="text-sm text-muted-foreground">{status}</p>
    {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
  </div>
}
