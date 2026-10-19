import { createFileRoute } from '@tanstack/react-router'
import { useState } from 'react'
import { Bell, LogOut, Palette } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useDevice } from '@/features/identity/context'
import { useWorkspace } from '@/features/workspace/context'
import { readTheme, setTheme, type Theme } from '@/lib/theme'

export const Route = createFileRoute('/_app/settings')({ component: SettingsPage })

const themes: Array<[Theme, string]> = [['system', '跟随系统'], ['light', '浅色'], ['dark', '深色']]

function SettingsPage() {
  const workspace = useWorkspace()
  const device = useDevice()
  const [theme, setThemeState] = useState(readTheme)
  return <div className="mx-auto grid max-w-2xl gap-6">
    <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><Bell aria-hidden="true" className="size-5" />提醒</h2>
      <Button type="button" variant="secondary" className="w-fit" disabled={workspace.notificationButton.disabled} onClick={() => { void workspace.run(() => workspace.enableBrowserNotifications()) }}>{workspace.notificationButton.label}</Button>
    </section>
    <section className="grid gap-3">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><Palette aria-hidden="true" className="size-5" />外观</h2>
      <div role="radiogroup" aria-label="外观" className="flex w-fit gap-1 rounded-lg bg-muted p-1">
        {themes.map(([value, label]) => <Button key={value} type="button" role="radio" aria-checked={theme === value} variant={theme === value ? 'outline' : 'ghost'} onClick={() => { setTheme(value); setThemeState(value) }}>{label}</Button>)}
      </div>
    </section>
    <section className="border-t pt-6">
      <Button type="button" variant="ghost" className="text-muted-foreground" onClick={() => device.disconnect()}><LogOut aria-hidden="true" />断开这个浏览器</Button>
    </section>
  </div>
}
