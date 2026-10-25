import { useEffect, useState, type ReactNode } from 'react'
import { Check, Copy, Globe, Info, Server, Wifi, WifiOff, X } from 'lucide-react'
import { cn } from '@/lib/utils'
import { Details, InlineError, SettingsDisclosure, SettingsRow } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import type { ConnectionEntry } from '@vibeface/client/lib/node.ts'
import { addressName, hostOf } from '@vibeface/client/workspace/labels.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { useDevice } from '@vibeface/client/identity/context.tsx'

const keyOf = (entry: ConnectionEntry) => `${entry.address}|${entry.device_id}|${entry.local === true}`

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

/** Settings → 连接: where this browser connects and other saved addresses; ids only as copied diagnostics. */
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
      <Button type="button" variant="ghost" size="icon" aria-label="移除" title="移除" onClick={() => { try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </SettingsRow>)}
    {error ? <div className="px-4 py-3"><InlineError error={error} /></div> : null}
    <SettingsDisclosure icon={Info} label="详细信息">
      <CopyDiagnostics lines={[['个人网络', directory?.network_id || ready?.principal.network_id], ['授权版本', directory?.epoch], ['这个浏览器', identity?.deviceId], ['当前连接', ready?.entry.device_id]]} />
      <Details label="手动添加地址">
        <form className="grid gap-2" noValidate onSubmit={(event) => {
          event.preventDefault(); const form = event.currentTarget
          const field = (name: string) => (form.elements.namedItem(name) as HTMLInputElement).value.trim()
          try { device.saveEntry({ address: field('address'), device_id: field('device_id') }); form.reset(); setError(null) } catch (cause) { setError(cause) }
        }}>
          <Input type="url" name="address" aria-label="访问地址" placeholder="https://" />
          <Input className="font-mono" name="device_id" aria-label="设备编号" placeholder="设备编号" autoComplete="off" autoCapitalize="none" spellCheck={false} />
          <Button type="submit" variant="outline" className="w-fit">保存</Button>
        </form>
      </Details>
    </SettingsDisclosure>
  </>
}
