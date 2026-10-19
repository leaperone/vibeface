import { useState } from 'react'
import { Globe, X } from 'lucide-react'
import { Details, DetailRow, InlineError } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import type { ConnectionEntry } from '@/lib/node'
import { addressName } from '@/features/workspace/labels'
import { useDevice } from './context'

const keyOf = (entry: ConnectionEntry) => `${entry.address}|${entry.peer_id}|${entry.local === true}`

/** Settings → 连接: where this browser connects, other saved addresses and the raw ids. */
export function ConnectionSettings() {
  const device = useDevice()
  const { ready, identity, entries, directory } = device
  const [error, setError] = useState<unknown>(null)
  const current = ready ? keyOf(ready.entry) : ''
  const others = entries.filter((entry) => keyOf(entry) !== current)
  return <div className="grid gap-3">
    {ready ? <p className="flex min-w-0 items-center gap-2"><Globe aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="truncate" title={ready.entry.address}>{addressName(ready.entry.address)}</span></p> : null}
    {others.length ? <ul className="grid gap-1">{others.map((entry) => <li key={keyOf(entry)} className="flex min-h-11 items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <span className="min-w-0 flex-1 truncate" title={entry.address}>{addressName(entry.address)}</span>
      <Button type="button" variant="ghost" onClick={() => { setError(null); void device.connect(entry).catch(setError) }}>切换</Button>
      <Button type="button" variant="ghost" size="icon" aria-label="移除" onClick={() => { try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </li>)}</ul> : null}
    <InlineError error={error} />
    <Details label="手动添加地址">
      <form className="grid gap-2" onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        const field = (name: string) => (form.elements.namedItem(name) as HTMLInputElement).value.trim()
        try { device.saveEntry({ address: field('address'), peer_id: field('peer_id') }); form.reset(); setError(null) } catch (cause) { setError(cause) }
      }}>
        <Input type="url" name="address" required aria-label="访问地址" placeholder="https://" />
        <Input className="font-mono" name="peer_id" required pattern="[a-f0-9]{64}" aria-label="设备编号" placeholder="设备编号" autoComplete="off" autoCapitalize="none" spellCheck={false} />
        <Button type="submit" variant="outline" className="w-fit">保存</Button>
      </form>
    </Details>
    <Details>
      <DetailRow label="个人网络" value={directory?.network_id || ready?.principal.network_id || ''} mono />
      {directory ? <DetailRow label="授权版本" value={directory.epoch} /> : null}
      <DetailRow label="这个浏览器的设备编号" value={identity?.peerId ?? ''} mono />
      <DetailRow label="这个浏览器的公钥" value={identity?.publicKey ?? ''} mono />
      {ready ? <DetailRow label="当前连接的设备编号" value={ready.entry.peer_id} mono /> : null}
    </Details>
  </div>
}
