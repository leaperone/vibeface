import { useEffect, useState, type ReactNode } from 'react'
import { Check, Copy, Wifi, WifiOff, X } from 'lucide-react'
import { terms } from '@/lib/glossary'
import { Details, InlineError, SettingsDisclosure } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { entryAddress, entryKey, parseEntry, type ConnectionEntry } from '@vibeface/client/lib/node.ts'
import { addressName, hostOf } from '@vibeface/client/workspace/labels.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { parseCustomHttpsBaseInput, publicBootstrapDefaults } from '@vibeface/client/lib/bootstrap.ts'

/** The ids support asks for, copied as one block instead of shown. */
export function CopyDiagnostics({ lines, label = '复制诊断信息' }: { lines: Array<[string, ReactNode]>; label?: string }) {
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  const text = lines.filter(([, value]) => value !== '' && value != null).map(([label, value]) => `${label}: ${value}`).join('\n')
  return <Button type="button" variant="outline" className="w-fit" onClick={() => { void navigator.clipboard?.writeText(text).then(() => setCopied(true)) }}>
    {copied ? <Check aria-hidden="true" className="text-success" /> : <Copy aria-hidden="true" />}<span role={copied ? 'status' : undefined}>{copied ? '已复制' : label}</span>
  </Button>
}

const deviceIdHex = /^[a-f0-9]{64}$/

/**
 * Settings → 高级 → 连接: where this browser connects, named once; the sidebar already shows it and whether it is online.
 * Folded under it, the saved addresses to drop (switching lives in the sidebar), the ids support asks for, and adding an
 * address by hand.
 */
export function ConnectionSettings() {
  const device = useDevice()
  const { connectionOnline } = useWorkspace()
  const { ready, identity, entries, directory, publicBootstrap, setPublicBootstrapMode } = device
  const [error, setError] = useState<unknown>(null)
  const [customFormActive, setCustomFormActive] = useState(() => publicBootstrap.kind === 'custom')
  const [baseDraft, setBaseDraft] = useState(() => (publicBootstrap.kind === 'custom' ? publicBootstrap.https_base : ''))
  const [issuerDraft, setIssuerDraft] = useState(() => (publicBootstrap.kind === 'custom' ? publicBootstrap.issuer : ''))

  useEffect(() => {
    if (publicBootstrap.kind === 'custom') {
      setCustomFormActive(true)
      setBaseDraft(publicBootstrap.https_base)
      setIssuerDraft(publicBootstrap.issuer)
    }
  }, [publicBootstrap])

  if (!ready) return null
  const current = entryKey(ready.entry)
  const others = entries.filter((entry) => entryKey(entry) !== current)
  const name = (entry: ConnectionEntry) => device.entryName(entry) || addressName(entryAddress(entry))
  const selectValue = publicBootstrap.kind === 'custom' || customFormActive ? 'custom' : publicBootstrap.kind

  return <SettingsDisclosure icon={connectionOnline ? Wifi : WifiOff} label="连接" value={<span title={entryAddress(ready.entry)}>{name(ready.entry)}</span>}>
    <p className="flex min-w-0 items-center justify-between gap-3 text-meta"><span className="shrink-0 text-muted-foreground">{terms.address.label}</span><span className="truncate" title={entryAddress(ready.entry)}>{hostOf(entryAddress(ready.entry)) || entryAddress(ready.entry)}</span></p>
    {others.length ? <ul className="grid gap-1" aria-label="用过的地址">{others.map((entry) => <li key={entryKey(entry)} className="flex min-h-9 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3 text-meta">
      <span className="min-w-0 flex-1 truncate" title={entryAddress(entry)}>{name(entry)}</span>
      <Button type="button" variant="ghost" size="icon" aria-label="从列表删掉这个地址" title="从列表删掉这个地址" onClick={() => { try { device.removeEntry(entry) } catch (cause) { setError(cause) } }}><X aria-hidden="true" /></Button>
    </li>)}</ul> : null}
    <CopyDiagnostics lines={[['个人网络', directory?.network_id || ready.principal.network_id], ['授权版本', directory?.epoch], ['这个浏览器', identity?.deviceId], ['当前连接', ready.entry.device_id]]} />
    <Details label="公共发现">
      <div className="grid gap-2">
        <p className="text-meta text-muted-foreground">
          在局域网直连之后，如仍连不上，可自动从公共目录尝试可用的网关入口；仅使用你已授权的节点，不会授予成员权限。
        </p>
        <NativeSelect
          size="sm"
          className="w-full max-w-md"
          aria-label="公共发现目录"
          value={selectValue}
          onChange={(event) => {
            const value = event.target.value
            if (value === 'default') {
              setCustomFormActive(false)
              setPublicBootstrapMode({ kind: 'default' })
              setError(null)
              return
            }
            if (value === 'off') {
              setCustomFormActive(false)
              setPublicBootstrapMode({ kind: 'off' })
              setError(null)
              return
            }
            setCustomFormActive(true)
            if (publicBootstrap.kind !== 'custom') {
              setBaseDraft('')
              setIssuerDraft('')
            }
          }}
        >
          <NativeSelectOption value="default">默认（{publicBootstrapDefaults.https_base}）</NativeSelectOption>
          <NativeSelectOption value="off">关闭</NativeSelectOption>
          <NativeSelectOption value="custom">自定义来源</NativeSelectOption>
        </NativeSelect>
        {selectValue === 'custom' ? <div className="grid gap-2">
          <Input
            className="font-mono"
            aria-label="签发者设备编号"
            placeholder="64 位十六进制签发者编号"
            value={issuerDraft}
            onChange={(event) => setIssuerDraft(event.target.value.trim().toLowerCase())}
            autoComplete="off"
            spellCheck={false}
          />
          <Input
            type="url"
            aria-label="发现服务 HTTPS 根地址"
            placeholder="https://example.com/bootstrap"
            value={baseDraft}
            onChange={(event) => setBaseDraft(event.target.value)}
          />
          <Button
            type="button"
            variant="outline"
            className="w-fit"
            onClick={() => {
              const issuer = issuerDraft.trim().toLowerCase()
              if (!deviceIdHex.test(issuer)) {
                setError(new Error('请填写有效的 64 位十六进制签发者编号'))
                return
              }
              const https_base = parseCustomHttpsBaseInput(baseDraft)
              if (!https_base) {
                setError(new Error('请填写有效的 HTTPS 根地址；本地夹具仅接受 http://127.0.0.1 或 localhost'))
                return
              }
              try {
                setPublicBootstrapMode({ kind: 'custom', issuer, https_base })
                setError(null)
              } catch (cause) {
                setError(cause)
              }
            }}
          >保存自定义来源</Button>
        </div> : null}
      </div>
    </Details>
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
