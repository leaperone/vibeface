import { useMemo } from 'react'
import { messageOf } from '@/lib/node'
import { createEnrollmentQr } from '@/lib/qr'

export function EnrollmentQr({ link }: { link: string }) {
  const code = useMemo(() => {
    try { return { kind: 'ready', ...createEnrollmentQr(link) } satisfies { kind: 'ready'; src: string; size: number } }
    catch (error) { return { kind: 'error', error: messageOf(error) } satisfies { kind: 'error'; error: string } }
  }, [link])
  return <div className="space-y-3">
    {code.kind === 'ready' ? <figure className="space-y-2"><img className="h-auto max-w-full rounded-lg border border-border" src={code.src} width={code.size} height={code.size} alt="使用新设备的相机扫描此邀请二维码" style={{ imageRendering: 'pixelated' }} /><figcaption className="text-sm text-muted-foreground">用新设备的相机扫描，再在原设备上核对并批准新设备。</figcaption></figure> : <p role="alert" className="text-sm text-destructive">{code.error}</p>}
    <label className="grid gap-1 text-sm">设备邀请链接<textarea className="min-h-24 w-full rounded-lg border border-input bg-background p-3 font-mono text-xs" readOnly value={link} onFocus={event => event.target.select()} /></label>
  </div>
}
