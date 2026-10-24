import { useState } from 'react'
import { Globe, Info, Server, Wifi, WifiOff, X } from 'lucide-react'
import { cn } from 'cn'
import { DetailRow, InlineError, SettingsDisclosure, SettingsRow } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import type { ConnectionEntry } from '@/lib/node'
import { addressName, hostOf } from '@/features/workspace/labels'
import { useWorkspace } from '@/features/workspace/context'
import { useDevice } from './context'

const keyOf = (entry: ConnectionEntry) => `${entry.address}|${entry.device_id}|${entry.local === true}`

/** Settings → 连接: where this browser connects, other saved addresses and the raw ids. */
export function ConnectionSettings() {
  const device = useDevice()
  const { connectionOnline } = useWorkspace()
  const { ready, identity, entries, directory } = device
  const [error, setError] = useState<unknown>(null)
  const current = ready ? keyOf(ready.entry) : ''
  const others = entries.filter((entry) => keyOf(entry) !== current)
  return <>
    {ready ? <SettingsRow icon={Globe} label={<span title={ready.entry.address}>{device.entryName(ready.entry) || addressName(ready.entry.address)}</span>} description={hostOf(ready.entry.address)}>
      <span className={cn('flex items-center gap-1.5', connectionOnline ? 'text-success' : 'text-muted-foreground')}>{connectionOnline ? <Wifi aria-hidden="true" className="size-4" /> : <WifiOff aria-hidden="true" className="size-4" />}{connectionOnline ? '在线' : '离线'}</span>
    </SettingsRow> : null}
    {others.map((entry) => <SettingsRow key={keyOf(entry)} icon={Server} label={<span title={entry.address}>{device.entryName(entry) || addressName(entry.address)}</span>} description={hostOf(entry.address)}>
      <Button type="button" size="sm" variant="outline" onClick={() => { setError(null); void device.connect(entry).catch(setError) }}>切换</Button>
      <Button type="button" variant="ghost" size="icon" className="size-8" aria-label="移除" title="移除" onClick={() => { try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </SettingsRow>)}
    {error ? <div className="px-4 py-3"><InlineError error={error} /></div> : null}
    <SettingsDisclosure icon={Info} label="详细信息">
      <div className="grid gap-3 text-meta text-muted-foreground">
        <DetailRow label="个人网络" value={directory?.network_id || ready?.principal.network_id || ''} mono />
        {directory ? <DetailRow label="授权版本" value={directory.epoch} /> : null}
        <DetailRow label="这个浏览器的设备编号" value={identity?.deviceId ?? ''} mono />
        {ready ? <DetailRow label="当前连接的设备编号" value={ready.entry.device_id} mono /> : null}
      </div>
      <form className="grid gap-2 border-t pt-3 text-meta" onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        const field = (name: string) => (form.elements.namedItem(name) as HTMLInputElement).value.trim()
        try { device.saveEntry({ address: field('address'), device_id: field('device_id') }); form.reset(); setError(null) } catch (cause) { setError(cause) }
      }}>
        <span className="text-foreground">手动添加地址</span>
        <Input type="url" name="address" required aria-label="访问地址" placeholder="https://" />
        <Input className="font-mono" name="device_id" required pattern="[a-f0-9]{64}" aria-label="设备编号" placeholder="设备编号" autoComplete="off" autoCapitalize="none" spellCheck={false} />
        <Button type="submit" variant="outline" className="w-fit">保存</Button>
      </form>
    </SettingsDisclosure>
  </>
}
