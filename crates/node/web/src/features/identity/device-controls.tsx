import { useState } from 'react'
import { Globe, Wifi, WifiOff, X } from 'lucide-react'
import { cn } from 'cn'
import { Details, DetailRow, InlineError } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import type { ConnectionEntry } from '@/lib/node'
import { addressName } from '@/features/workspace/labels'
import { useWorkspace } from '@/features/workspace/context'
import { useDevice } from './context'

const hostOf = (address: string) => { try { return new URL(address).host } catch { return '' } }
const keyOf = (entry: ConnectionEntry) => `${entry.address}|${entry.device_id}|${entry.local === true}`

/** Settings → 连接: where this browser connects, other saved addresses and the raw ids. */
export function ConnectionSettings() {
  const device = useDevice()
  const { connectionOnline } = useWorkspace()
  const { ready, identity, entries, directory } = device
  const [error, setError] = useState<unknown>(null)
  const current = ready ? keyOf(ready.entry) : ''
  const others = entries.filter((entry) => keyOf(entry) !== current)
  return <div className="grid gap-3">
    {ready ? <p className="flex min-h-11 min-w-0 items-center gap-2 rounded-lg bg-muted/60 px-3">
      <Globe aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 truncate" title={ready.entry.address}>{device.entryName(ready.entry) || addressName(ready.entry.address)}<span className="ms-2 text-meta text-muted-foreground">{hostOf(ready.entry.address)}</span></span>
      <span title={connectionOnline ? '在线' : '离线'} className={cn('flex shrink-0', connectionOnline ? 'text-success' : 'text-muted-foreground')}>{connectionOnline ? <Wifi aria-hidden="true" className="size-4" /> : <WifiOff aria-hidden="true" className="size-4" />}<span className="sr-only">{connectionOnline ? '在线' : '离线'}</span></span>
      <Button type="button" variant="ghost" size="icon" aria-label="移除" title="断开并移除" onClick={() => { const entry = ready.entry; device.disconnect(); try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </p> : null}
    {others.length ? <ul className="grid gap-1">{others.map((entry) => <li key={keyOf(entry)} className="flex min-h-11 items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <span className="min-w-0 flex-1 truncate" title={entry.address}>{device.entryName(entry) || addressName(entry.address)}<span className="ms-2 text-meta text-muted-foreground">{hostOf(entry.address)}</span></span>
      <Button type="button" variant="ghost" onClick={() => { setError(null); void device.connect(entry).catch(setError) }}>切换</Button>
      <Button type="button" variant="ghost" size="icon" aria-label="移除" onClick={() => { try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </li>)}</ul> : null}
    <InlineError error={error} />
    <Details>
      <DetailRow label="个人网络" value={directory?.network_id || ready?.principal.network_id || ''} mono />
      {directory ? <DetailRow label="授权版本" value={directory.epoch} /> : null}
      <DetailRow label="这个浏览器的设备编号" value={identity?.deviceId ?? ''} mono />
      {ready ? <DetailRow label="当前连接的设备编号" value={ready.entry.device_id} mono /> : null}
      <form className="grid gap-2 border-t pt-3" onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        const field = (name: string) => (form.elements.namedItem(name) as HTMLInputElement).value.trim()
        try { device.saveEntry({ address: field('address'), device_id: field('device_id') }); form.reset(); setError(null) } catch (cause) { setError(cause) }
      }}>
        <span className="text-foreground">手动添加地址</span>
        <Input type="url" name="address" required aria-label="访问地址" placeholder="https://" />
        <Input className="font-mono" name="device_id" required pattern="[a-f0-9]{64}" aria-label="设备编号" placeholder="设备编号" autoComplete="off" autoCapitalize="none" spellCheck={false} />
        <Button type="submit" variant="outline" className="w-fit">保存</Button>
      </form>
    </Details>
  </div>
}
