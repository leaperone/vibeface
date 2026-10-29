import { useEffect, useState, type ReactNode } from 'react'
import { Check, Copy, Wifi, WifiOff, X } from 'lucide-react'
import { cn } from '@/lib/utils'
import { terms } from '@/lib/glossary'
import { Details, InlineError, SettingsDisclosure } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { entryAddress, entryKey, parseEntry, type ConnectionEntry } from '@vibeface/client/lib/node.ts'
import { addressName, hostOf } from '@vibeface/client/workspace/labels.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { useDevice } from '@vibeface/client/identity/context.tsx'

/** The ids support asks for, copied as one block instead of shown. */
export function CopyDiagnostics({ lines }: { lines: Array<[string, ReactNode]> }) {
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  const text = lines.filter(([, value]) => value !== '' && value != null).map(([label, value]) => `${label}: ${value}`).join('\n')
  return <Button type="button" variant="outline" className="w-fit" onClick={() => { void navigator.clipboard?.writeText(text).then(() => setCopied(true)) }}>
    {copied ? <Check aria-hidden="true" className="text-success" /> : <Copy aria-hidden="true" />}<span role={copied ? 'status' : undefined}>{copied ? '已复制' : '复制诊断信息'}</span>
  </Button>
}

/**
 * Settings → 连接: one row for where this browser connects, its status as the sidebar shows it. Folded under it, the saved
 * addresses to drop (switching lives in the sidebar), the ids support asks for, and adding an address by hand.
 */
export function ConnectionSettings() {
  const device = useDevice()
  const { connectionOnline } = useWorkspace()
  const { ready, identity, entries, directory } = device
  const [error, setError] = useState<unknown>(null)
  if (!ready) return null
  const current = entryKey(ready.entry)
  const others = entries.filter((entry) => entryKey(entry) !== current)
  const name = (entry: ConnectionEntry) => device.entryName(entry) || addressName(entryAddress(entry))
  return <SettingsDisclosure icon={terms.thisComputer.icon} label={<span title={entryAddress(ready.entry)}>{name(ready.entry)}</span>}
    value={<span className={cn('flex items-center gap-1.5', connectionOnline ? 'text-success' : 'text-muted-foreground')}>{connectionOnline ? <Wifi aria-hidden="true" className="size-4" /> : <WifiOff aria-hidden="true" className="size-4" />}{connectionOnline ? '在线' : '离线'}</span>}>
    <p className="flex min-w-0 items-center justify-between gap-3 text-meta"><span className="shrink-0 text-muted-foreground">{terms.address.label}</span><span className="truncate" title={entryAddress(ready.entry)}>{hostOf(entryAddress(ready.entry)) || entryAddress(ready.entry)}</span></p>
    {others.length ? <ul className="grid gap-1" aria-label="用过的地址">{others.map((entry) => <li key={entryKey(entry)} className="flex min-h-9 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3 text-meta">
      <span className="min-w-0 flex-1 truncate" title={entryAddress(entry)}>{name(entry)}</span>
      <Button type="button" variant="ghost" size="icon" aria-label="从列表删掉这个地址" title="从列表删掉这个地址" onClick={() => { try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </li>)}</ul> : null}
    <CopyDiagnostics lines={[['个人网络', directory?.network_id || ready.principal.network_id], ['授权版本', directory?.epoch], ['这个浏览器', identity?.deviceId], ['当前连接', ready.entry.device_id]]} />
    <Details label="手动添加地址">
      <form className="grid gap-2" noValidate onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        const field = (name: string) => (form.elements.namedItem(name) as HTMLInputElement).value.trim()
        try { device.saveEntry(parseEntry({ address: field('address'), device_id: field('device_id') })); form.reset(); setError(null) } catch (cause) { setError(cause) }
      }}>
        <Input type="url" name="address" aria-label="访问地址" placeholder="https://" />
        <Input className="font-mono" name="device_id" aria-label="设备编号" placeholder="设备编号" autoComplete="off" autoCapitalize="none" spellCheck={false} />
        <Button type="submit" variant="outline" className="w-fit">保存</Button>
      </form>
    </Details>
    {error ? <InlineError error={error} /> : null}
  </SettingsDisclosure>
}
