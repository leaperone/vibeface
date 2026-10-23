import { Link, createFileRoute } from '@tanstack/react-router'
import { useState } from 'react'
import { Bell, FileKey, ChevronRight, Download, Globe, LifeBuoy, LogOut, Palette, ScrollText } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { ConnectionSettings } from '@/features/identity/device-controls'
import { TriageSettings } from '@/features/devices/triage-settings'
import { CurrentRecoveryFile, RecoveryControls } from '@/features/identity/recovery-controls'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { readTheme, setTheme, type Theme } from '@/lib/theme'

export const Route = createFileRoute('/_app/settings')({ component: SettingsPage })

const ranges: Array<[number, string]> = [[3_600_000, '最近 1 小时'], [86_400_000, '最近 1 天'], [7 * 86_400_000, '最近 7 天']]

const themes: Array<[Theme, string]> = [['system', '跟随系统'], ['light', '浅色'], ['dark', '深色']]

function SettingsPage() {
  const workspace = useWorkspace()
  const device = useDevice()
  const [theme, setThemeState] = useState(readTheme)
  const [leaving, setLeaving] = useState(false)
  const [range, setRange] = useState(86_400_000)
  return <div className="mx-auto grid w-full max-w-3xl gap-6">
    <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><Bell aria-hidden="true" className="size-5" />提醒</h2>
      <Button type="button" variant="secondary" className="w-fit" disabled={workspace.notificationButton.disabled} onClick={() => { void workspace.run(() => workspace.enableNotifications(), "开启提醒") }}>{workspace.notificationButton.label}</Button>
      {device.ready?.principal.can_manage || device.ready?.principal.source === 'local_session' ? <TriageSettings /> : null}
    </section>
    <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><Palette aria-hidden="true" className="size-5" />外观</h2>
      <div role="radiogroup" aria-label="外观" className="flex w-fit gap-1 rounded-lg bg-muted p-1">
        {themes.map(([value, label]) => <Button key={value} type="button" role="radio" aria-checked={theme === value} variant={theme === value ? 'outline' : 'ghost'} onClick={() => { setTheme(value); setThemeState(value) }}>{label}</Button>)}
      </div>
    </section>
    <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><Globe aria-hidden="true" className="size-5" />连接</h2>
      <ConnectionSettings />
    </section>
    {device.ready?.principal.can_manage ? <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><FileKey aria-hidden="true" className="size-5" />恢复文件</h2>
      <CurrentRecoveryFile />
      <RecoveryControls />
    </section> : null}
    {device.ready?.principal.can_manage || device.ready?.principal.source === 'local_session' ? <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><LifeBuoy aria-hidden="true" className="size-5" />排障</h2>
      <Button asChild variant="outline" className="h-auto justify-start gap-3 py-3 max-md:h-auto">
        <Link to="/settings/logs"><ScrollText aria-hidden="true" /><span className="flex-1 text-left">日志</span><ChevronRight aria-hidden="true" className="text-muted-foreground" /></Link>
      </Button>
      <p className="text-meta text-muted-foreground">诊断包不含登录信息和加入链接</p>
      <div className="flex flex-wrap items-center gap-2">
        <NativeSelect aria-label="诊断包的时间范围" value={String(range)} onChange={(event) => setRange(Number(event.target.value))} className="[&_select]:h-9 max-md:[&_select]:h-11">
          {ranges.map(([value, label]) => <NativeSelectOption key={value} value={String(value)}>{label}</NativeSelectOption>)}
        </NativeSelect>
        <Button type="button" variant="secondary" className="w-fit" onClick={() => { void workspace.run(async () => saveDiagnostics(await workspace.diagnostics(Date.now() - range)), "导出诊断包") }}><Download aria-hidden="true" />导出诊断包</Button>
      </div>
    </section> : null}
    <section className="grid gap-3 border-t pt-6">
      {leaving ? <>
        <p>断开后，要重新打开本机页面或用加入链接才能回来。</p>
        <div className="flex flex-wrap gap-2">
          <Button type="button" variant="ghost" onClick={() => setLeaving(false)}>取消</Button>
          <Button type="button" variant="destructive" onClick={() => device.disconnect()}><LogOut aria-hidden="true" />断开</Button>
        </div>
      </> : <Button type="button" variant="ghost" className="-ms-3 w-fit text-muted-foreground" onClick={() => setLeaving(true)}><LogOut aria-hidden="true" />断开这个浏览器</Button>}
    </section>
  </div>
}

function saveDiagnostics(bundle: unknown) {
  const url = URL.createObjectURL(new Blob([JSON.stringify(bundle, null, 2)], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = `vibeface-diagnostics-${new Date().toLocaleDateString('sv')}.json`
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
