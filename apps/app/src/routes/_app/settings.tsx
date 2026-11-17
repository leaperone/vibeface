import { BusyIcon,CountBadge,Segmented,SegmentedValue,SettingsGroup,SettingsRow } from '@/components/arc/product-kit'
import { RoutedAction } from "@/components/arc/routed-action"
import { useOpenJoins } from '@/features/devices/join'
import { TriageSettings } from '@/features/devices/triage-settings'
import { ConnectionSettings } from '@/features/identity/device-controls'
import { CurrentRecoveryFile,PhoneRecovery,RecoveredStep,RecoveryControls,RecoveryReview } from '@/features/identity/recovery-controls'
import { terms } from '@/lib/glossary'
import { readTheme,setTheme,type Theme } from '@/lib/theme'
import { cn } from '@/lib/utils'
import { Link,createFileRoute } from '@tanstack/react-router'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { onlyManagerText,securityCopy,soleManager,waitingCopy } from '@vibeface/client/identity/copy.ts'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Drawer,DrawerContent } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Select } from "@vibeface/uiarc/select/select.tsx"
import { ArrowUpRight,Bell,BellOff,BookOpen,Check,ChevronRight,CircleAlert,CircleCheck,Download,FlaskConical,LifeBuoy,LogOut,Monitor,Moon,Palette,Plus,ScrollText,ShieldAlert,ShieldCheck,Sun,TriangleAlert,type LucideIcon } from 'lucide-react'
import { useEffect,useRef,useState,type ReactNode } from 'react'

export const Route = createFileRoute('/_app/settings')({ component: SettingsPage })

const ranges: Array<[number, string]> = [[3_600_000, '近 1 小时'], [86_400_000, '近 1 天'], [7 * 86_400_000, '近 7 天']]

const themes: Array<{ value: Theme; label: string; icon: LucideIcon }> = [{ value: 'system', label: '跟随系统', icon: Monitor }, { value: 'light', label: '浅色', icon: Sun }, { value: 'dark', label: '深色', icon: Moon }]

function SettingsPage() {
  const workspace = useWorkspace()
  const device = useDevice()
  const [theme, setThemeState] = useState(readTheme)
  const [leaving, setLeaving] = useState(false)
  const leaveTrigger = useRef<HTMLButtonElement>(null)
  const [range, setRange] = useState(86_400_000)
  const [exported, setExported] = useState<'idle' | 'busy' | 'done'>('idle')
  useEffect(() => {
    if (exported !== 'done') return
    const timer = setTimeout(() => setExported('idle'), 2000)
    return () => clearTimeout(timer)
  }, [exported])
  const principal = device.ready?.principal
  const operator = principal?.can_manage || principal?.source === 'local_session'
  // A local session only disconnects, so only a member browser can be the one manager left.
  const sole = principal?.source !== 'local_session' && soleManager(device.directory?.members ?? [], device.identity?.deviceId)
  const otherClient = device.directory?.members.some((member) => member.kind === 'client' && member.device_id !== device.identity?.deviceId)
  const joins = useOpenJoins()
  const offline = workspace.nodes.some((node) => !node.revoked && !node.online)
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <SettingsGroup>
      <Link to="/devices" className="interactive-row block">
        <SettingsRow icon={terms.myDevices.icon} label={terms.myDevices.label}>
          {joins.length ? <CountBadge count={joins.length} label={`${joins.length} 个加入请求`} /> : offline ? <CircleAlert role="img" aria-label="有设备离线" className="size-4 text-warning" /> : <span className="tabular-nums">{device.directory?.members.length ?? ''}</span>}
          <ChevronRight aria-hidden="true" className="size-4" />
        </SettingsRow>
      </Link>
    </SettingsGroup>
    <SettingsGroup title={waitingCopy.title}>
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
    {principal?.can_manage ? <SecurityStatus>
      {device.review?.kind === 'recovery' || device.recovered ? null : <><CurrentRecoveryFile /><PhoneRecovery /><RecoveryControls /></>}
    </SecurityStatus> : null}
    {principal?.can_manage ? device.review?.kind === 'recovery' ? <RecoveryReview /> : device.recovered ? <RecoveredStep /> : null : null}
    <SettingsGroup title="帮助" footer={<a href="https://github.com/leaperone/vibeface/tree/main/packages/docs" target="_blank" rel="noopener noreferrer" className="inline-flex min-h-11 items-center underline underline-offset-4">也可在 GitHub 查看源文档<span className="sr-only">，在新标签页打开</span></a>}>
      <a href="https://www.vibeface.dev/docs/user-guide" target="_blank" rel="noopener noreferrer" className="interactive-row block">
        <SettingsRow icon={BookOpen} label="使用指南" description="查看 Bot、任务和设备的操作步骤"><ArrowUpRight aria-hidden="true" className="size-4" /><span className="sr-only">在新标签页打开</span></SettingsRow>
      </a>
      <a href="https://www.vibeface.dev/docs/troubleshooting" target="_blank" rel="noopener noreferrer" className="interactive-row block">
        <SettingsRow icon={LifeBuoy} label="连接与恢复排查" description="连不上设备或结果未确认时，按当前状态排查"><ArrowUpRight aria-hidden="true" className="size-4" /><span className="sr-only">在新标签页打开</span></SettingsRow>
      </a>
    </SettingsGroup>
    <SettingsGroup title="高级">
      <ConnectionSettings />
      <Link to="/settings/lab" className="interactive-row block">
        <SettingsRow icon={FlaskConical} label="实验室" description="开启或关闭文件传输等试用功能"><ChevronRight aria-hidden="true" className="size-4" /></SettingsRow>
      </Link>
    </SettingsGroup>
    {operator ? <SettingsGroup title="问题排查" footer="诊断包不含登录信息和加入链接">
      <Link to="/settings/logs" className="interactive-row block">
        <SettingsRow icon={ScrollText} label="日志"><ChevronRight aria-hidden="true" className="size-4" /></SettingsRow>
      </Link>
      <SettingsRow icon={Download} label="诊断包">
        <span className="flex">
          <Select label="诊断包的时间范围" className="focus-within:z-10" value={"arc:" + String(range)} onValueChange={selected => { const value = selected.slice(4);  setRange(Number(value))  }} options={ranges.map(([value, label]) => ({ value: "arc:" + String(value), label: label }))} />
          <Button type="button" size="sm" variant="secondary" className="relative -ms-px focus-visible:z-10" disabled={exported === 'busy'} onClick={() => {
            setExported('busy')
            void workspace.run(async () => { saveDiagnostics(await workspace.diagnostics(Date.now() - range)); setExported('done') }, "导出诊断包")
              .finally(() => setExported((state) => state === 'busy' ? 'idle' : state))
          }}>{exported === 'done' ? <><Check aria-hidden="true" className="text-success" /><span role="status">已导出</span></> : <><BusyIcon busy={exported === 'busy'} />导出</>}</Button>
        </span>
      </SettingsRow>
    </SettingsGroup> : null}
    <SettingsGroup>
      <SettingsRow ref={leaveTrigger} icon={LogOut} tone="danger" label="断开这个浏览器" onClick={() => setLeaving(true)} />
    </SettingsGroup>
    <Drawer open={leaving} onOpenChange={setLeaving}>
      <DrawerContent title={sole ? onlyManagerText("这个浏览器") : "断开这个浏览器？"} description={sole ? "不能断开" : "要回来，得重新打开本机页面或用加入链接"} side="bottom" className="gap-0 pb-[env(safe-area-inset-bottom)] sm:mx-auto sm:max-w-md sm:rounded-t-2xl sm:border-x" onCloseAutoFocus={(event) => { event.preventDefault(); leaveTrigger.current?.focus({ preventScroll: true }) }}>
        {sole ? <>
          <div className="px-4 pt-4">


          </div>
          <div className="flex justify-end gap-2 px-4 pt-2 pb-4">
            <Button type="button" variant="ghost" onClick={() => setLeaving(false)}>知道了</Button>
            {/* With no other device to promote, the next manager has to join first. */}
            <RoutedAction to="/devices" search={otherClient ? {} : { add: true }}>{otherClient ? <terms.myDevices.icon aria-hidden="true" /> : <Plus aria-hidden="true" />}{otherClient ? terms.myDevices.label : '添加设备'}</RoutedAction>
          </div>
        </> : <>
          <div className="gap-1.5 px-4 pt-4">


          </div>
          <div className="flex justify-end gap-2 px-4 pt-2 pb-4">
            <Button type="button" variant="ghost" onClick={() => setLeaving(false)}>取消</Button>
            <Button type="button" variant="danger" onClick={() => { void workspace.run(async () => { setLeaving(false); await device.leave() }, '断开这个浏览器') }}>断开</Button>
          </div>
        </>}
      </DrawerContent>
    </Drawer>
  </div>
}

/**
 * 安全 in one group: how many devices can manage, whether a recovery waits (also after 是我 hid its reminder), then the
 * recovery file and password; `children` holds those controls.
 */
function SecurityStatus({ children }: { children: ReactNode }) {
  const { directory, nodeClockOffset } = useDevice()
  const managers = directory?.members.filter((member) => member.manage).length ?? 0
  const pending = directory?.pending
  const link = 'interactive-row block'
  return <SettingsGroup title={securityCopy.title}>
    {directory ? <><Link to="/devices" className={link}>
      <SettingsRow icon={terms.admin.icon} label={securityCopy.managers}>
        <span className={cn('inline-flex items-center gap-1 tabular-nums', managers === 1 && 'text-warning')}>{managers === 1 ? <TriangleAlert aria-hidden="true" className="size-4" /> : null}{securityCopy.managerCount(managers)}</span>
        <ChevronRight aria-hidden="true" className="size-4" />
      </SettingsRow>
    </Link>
    {pending ? <Link to="/tasks" className={link}>
      <SettingsRow icon={ShieldAlert} tone="danger" label={securityCopy.recovery}>
        <span className="text-danger tabular-nums">{securityCopy.recoveryIn(pending.effective_at_ms - nodeClockOffset)}</span>
        <ChevronRight aria-hidden="true" className="size-4" />
      </SettingsRow>
    </Link> : <SettingsRow icon={ShieldCheck} label={securityCopy.recovery}>{securityCopy.noRecovery}</SettingsRow>}</> : null}
    {children}
  </SettingsGroup>
}

const notificationText = { on: '已开启', unsupported: '不支持', denied: '已拒绝' } as const

function NotificationValue() {
  const workspace = useWorkspace()
  const { label, disabled, state } = workspace.notificationButton
  if (state === 'off' && !disabled) return <Button type="button" size="sm" variant="secondary" title={label} onClick={() => { void workspace.run(() => workspace.enableNotifications(), '开启提醒') }}>开启</Button>
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
