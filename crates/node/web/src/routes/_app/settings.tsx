import { Link, createFileRoute } from '@tanstack/react-router'
import { useEffect, useState } from 'react'
import { Bell, BellOff, Check, ChevronRight, CircleCheck, Download, LogOut, Monitor, Moon, Palette, ScrollText, Sun, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { BusyIcon, Segmented, SegmentedValue, SettingsGroup, SettingsRow } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { ConnectionSettings } from '@/features/identity/device-controls'
import { TriageSettings } from '@/features/devices/triage-settings'
import { CurrentRecoveryFile, RecoveredStep, RecoveryControls, RecoveryReview, useJustRecovered } from '@/features/identity/recovery-controls'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { readTheme, setTheme, type Theme } from '@/lib/theme'

export const Route = createFileRoute('/_app/settings')({ component: SettingsPage })

const ranges: Array<[number, string]> = [[3_600_000, '近 1 小时'], [86_400_000, '近 1 天'], [7 * 86_400_000, '近 7 天']]

const themes: Array<{ value: Theme; label: string; icon: LucideIcon }> = [{ value: 'system', label: '跟随系统', icon: Monitor }, { value: 'light', label: '浅色', icon: Sun }, { value: 'dark', label: '深色', icon: Moon }]

function SettingsPage() {
  const workspace = useWorkspace()
  const device = useDevice()
  const [theme, setThemeState] = useState(readTheme)
  const [leaving, setLeaving] = useState(false)
  const [range, setRange] = useState(86_400_000)
  const [exported, setExported] = useState<'idle' | 'busy' | 'done'>('idle')
  const recovered = useJustRecovered()
  useEffect(() => {
    if (exported !== 'done') return
    const timer = setTimeout(() => setExported('idle'), 2000)
    return () => clearTimeout(timer)
  }, [exported])
  const principal = device.ready?.principal
  const operator = principal?.can_manage || principal?.source === 'local_session'
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-8">
    <SettingsGroup title="提醒">
      <SettingsRow icon={Bell} label="浏览器提醒" description={workspace.notificationButton.state === 'denied' ? '在浏览器设置里允许' : undefined}>
        <NotificationValue />
      </SettingsRow>
      {operator ? <TriageSettings /> : null}
    </SettingsGroup>
    <SettingsGroup title="外观">
      <SettingsRow icon={Palette} label="主题" description={<SegmentedValue value={theme} options={themes} />}>
        <Segmented label="外观" value={theme} options={themes} onChange={(value) => { setTheme(value); setThemeState(value) }} />
      </SettingsRow>
    </SettingsGroup>
    <SettingsGroup title="连接"><ConnectionSettings /></SettingsGroup>
    {principal?.can_manage ? device.review?.kind === 'recovery' ? <RecoveryReview /> : recovered ? <RecoveredStep /> : <SettingsGroup title="恢复" footer="恢复文件只有一份有效，丢了不能补">
      <CurrentRecoveryFile />
      <RecoveryControls />
    </SettingsGroup> : null}
    {operator ? <SettingsGroup title="排障" footer="诊断包不含登录信息和加入链接">
      <Link to="/settings/logs" className="block transition-colors outline-none hover:bg-muted/50 focus-visible:bg-muted/60">
        <SettingsRow icon={ScrollText} label="日志"><ChevronRight aria-hidden="true" className="size-4" /></SettingsRow>
      </Link>
      <SettingsRow icon={Download} label="诊断包">
        <span className="flex">
          <NativeSelect size="sm" className="focus-within:z-10 [&_select]:rounded-e-none!" aria-label="诊断包的时间范围" value={String(range)} onChange={(event) => setRange(Number(event.target.value))}>
            {ranges.map(([value, label]) => <NativeSelectOption key={value} value={String(value)}>{label}</NativeSelectOption>)}
          </NativeSelect>
          <Button type="button" size="sm" variant="outline" className="relative -ms-px rounded-s-none focus-visible:z-10" disabled={exported === 'busy'} onClick={() => {
            setExported('busy')
            void workspace.run(async () => { saveDiagnostics(await workspace.diagnostics(Date.now() - range)); setExported('done') }, "导出诊断包")
              .finally(() => setExported((state) => state === 'busy' ? 'idle' : state))
          }}>{exported === 'done' ? <><Check aria-hidden="true" className="text-success" /><span role="status">已导出</span></> : <><BusyIcon busy={exported === 'busy'} />导出</>}</Button>
        </span>
      </SettingsRow>
    </SettingsGroup> : null}
    <SettingsGroup>
      <SettingsRow icon={LogOut} tone="danger" label="断开这个浏览器" onClick={() => setLeaving(true)} />
    </SettingsGroup>
    <Sheet open={leaving} onOpenChange={setLeaving}>
      <SheetContent side="bottom" showCloseButton={false} className="gap-0 pb-[env(safe-area-inset-bottom)] sm:mx-auto sm:max-w-md sm:rounded-t-2xl sm:border-x">
        <SheetHeader className="gap-1.5 px-5 pt-5">
          <SheetTitle className="flex items-center gap-2 text-heading font-semibold"><LogOut aria-hidden="true" className="size-5 text-danger" />断开这个浏览器？</SheetTitle>
          <SheetDescription>要回来，得重新打开本机页面或用加入链接</SheetDescription>
        </SheetHeader>
        <div className="flex justify-end gap-2 px-5 pt-2 pb-5">
          <Button type="button" variant="ghost" onClick={() => setLeaving(false)}>取消</Button>
          <Button type="button" variant="destructive" onClick={() => device.disconnect()}>断开</Button>
        </div>
      </SheetContent>
    </Sheet>
  </div>
}

const notificationText = { on: '已开启', unsupported: '不支持', denied: '已拒绝' } as const

function NotificationValue() {
  const workspace = useWorkspace()
  const { label, disabled, state } = workspace.notificationButton
  if (state === 'off' && !disabled) return <Button type="button" size="sm" variant="outline" title={label} onClick={() => { void workspace.run(() => workspace.enableNotifications(), '开启提醒') }}>开启</Button>
  return <span title={label} className={cn('inline-flex items-center gap-1', state === 'on' && 'text-success')}>
    {state === 'on' ? <CircleCheck aria-hidden="true" className="size-4" /> : state === 'denied' ? <BellOff aria-hidden="true" className="size-4" /> : null}
    {state === 'off' ? label : notificationText[state]}
  </span>
}

function saveDiagnostics(bundle: unknown) {
  const url = URL.createObjectURL(new Blob([JSON.stringify(bundle, null, 2)], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = `vibeface-diagnostics-${new Date().toLocaleDateString('sv')}.json`
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
