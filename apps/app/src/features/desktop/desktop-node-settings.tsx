import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ChevronRight, Laptop, RefreshCw } from 'lucide-react'
import { BusyIcon, SettingsGroup, SettingsRow } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { desktopNodeApi } from '@/lib/desktop-node-bridge'
import type { DesktopStatus } from '../../../../desktop/src/shared/contract.ts'
import { describeNodeSettings, type Action, type ActionId } from '../../../../desktop/src/shared/node-view.ts'

const RELEASES = 'https://github.com/leaperone/vibeface/releases'

export function DesktopNodeSettings() {
  const api = desktopNodeApi()
  const [status, setStatus] = useState<DesktopStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const reload = () => {
    if (!api || busy) return
    setError(null)
    setStatus(null)
    setBusy(true)
    void api.getStatus()
      .then((value) => setStatus(value))
      .catch((cause: unknown) => setError(cause instanceof Error ? cause.message : '无法读取本机节点状态。'))
      .finally(() => setBusy(false))
  }

  useEffect(() => {
    if (!api) return
    let live = true
    void api.getStatus()
      .then((value) => { if (live) { setStatus(value); setError(null) } })
      .catch((cause: unknown) => { if (live) setError(cause instanceof Error ? cause.message : '无法读取本机节点状态。') })
    const stop = api.onStatus((value) => { setStatus(value); setError(null) })
    return () => { live = false; stop() }
  }, [api])

  if (!api) {
    return <SettingsGroup title="本机节点">
      <p className="px-4 py-3 text-body text-muted-foreground">此页面仅在 Vibeface 桌面 App 中可用。</p>
    </SettingsGroup>
  }

  const hostStarting = status?.hostPhase === 'starting'

  const run = (work: () => Promise<DesktopStatus | void>, action?: ActionId) => {
    if (busy || hostStarting) return
    setBusy(true)
    setError(null)
    void work()
      .then((next) => {
        if (!next) return
        setStatus(next)
        if (action === 'open' || action === 'local') return
        const changing = new Set<string>(['controller', 'choose', 'connect', 'start-discovered', 'start-owned', 'stop-owned', 'install-background', 'start-background', 'stop-background', 'remove-background', 'update-background'])
        const selectedChanged = status?.selectedNode?.deviceId !== next.selectedNode?.deviceId || status?.selectedNode?.dataDir !== next.selectedNode?.dataDir
        if (!selectedChanged && (!action || !changing.has(action))) return
        if (action === 'choose' && !selectedChanged) return
        const stopping = next.selectedNode === null || action === 'stop-owned' || action === 'stop-background' || action === 'remove-background'
        api.reconnectClient(!stopping && next.preferences.openBusinessOnConnect)
      })
      .catch((cause: unknown) => setError(cause instanceof Error ? cause.message : '出现未知错误。'))
      .finally(() => setBusy(false))
  }

  const invoke = async (id: ActionId, nodeId?: string): Promise<DesktopStatus | void> => {
    switch (id) {
      case 'controller': return api.chooseNodeMode('controller')
      case 'local': return api.chooseNodeMode('local')
      case 'discover': return api.discoverNodes()
      case 'start-owned': return api.startOwned()
      case 'stop-owned': return api.stopOwned()
      case 'open':
        api.reconnectClient(true)
        return
      case 'choose': return api.chooseDataDir()
      case 'connect': return api.connectDiscoveredNode(nodeId ?? '')
      case 'start-discovered': return api.startDiscoveredNode(nodeId ?? '')
      case 'install-background': return api.installBackgroundNode()
      case 'start-background': return api.startBackgroundNode()
      case 'stop-background': return api.stopBackgroundNode()
      case 'remove-background': return api.removeBackgroundNode()
      case 'update-background': return api.updateBackgroundNode()
    }
  }

  const view = status ? describeNodeSettings(status) : null

  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <SettingsGroup title="本机节点" footer={view?.scope}>
      {view ? <>
        <SettingsRow icon={Laptop} label={view.headline} description={view.explain}>
          <span className="text-meta text-muted-foreground">{view.indicator}</span>
        </SettingsRow>
        {hostStarting ? <p className="px-4 text-meta text-muted-foreground">正在完成桌面启动，节点操作暂时不可用…</p> : null}
        {view.failure ? <p className="px-4 text-body text-danger" role="alert">{view.failure}</p> : null}
        {error ? <p className="px-4 text-body text-danger" role="alert">{error}</p> : null}
        {view.discoveryMessage ? <p className="px-4 text-meta text-muted-foreground">{view.discoveryMessage}</p> : null}
        <ActionList actions={view.actions} busy={busy || hostStarting} onAction={(action) => run(() => invoke(action.id, action.nodeId), action.id)} />
      </> : error ? <>
        <p className="px-4 text-body text-danger" role="alert">{error}</p>
        <div className="px-4 pb-4">
          <Button type="button" size="sm" variant="outline" disabled={busy} onClick={reload}>
            {busy ? <BusyIcon busy /> : null}
            重试
          </Button>
        </div>
      </> : <SettingsRow icon={RefreshCw} label="正在读取本机节点状态…"><BusyIcon busy /></SettingsRow>}
    </SettingsGroup>

    {view && view.rows.length > 0 ? <SettingsGroup title="已发现的节点">
      {view.rows.map((row) => <div key={row.node.id} className="border-b border-border px-4 py-3 last:border-b-0">
        <p className="text-body font-medium text-foreground">{row.status} · {row.node.deviceId.slice(0, 12)}</p>
        <p className="text-meta text-muted-foreground">{row.detail}</p>
        <p className="mt-1 break-all font-mono text-meta text-muted-foreground">{row.node.dataDir}</p>
        <ActionList actions={row.actions} busy={busy || hostStarting} onAction={(action) => run(() => invoke(action.id, action.nodeId), action.id)} className="mt-3" />
      </div>)}
    </SettingsGroup> : null}

    {view && (view.backgroundMessage || view.backgroundActions.length > 0) ? <SettingsGroup title="后台节点" footer={view.backgroundMessage}>
      <ActionList actions={view.backgroundActions} busy={busy || hostStarting} onAction={(action) => run(() => invoke(action.id, action.nodeId), action.id)} />
    </SettingsGroup> : null}

    {status ? <SettingsGroup title="桌面偏好">
      <SettingsRow icon={Laptop} label="连接后自动回到 Vibeface" description="选中节点后打开主界面">
        <input
          type="checkbox"
          role="switch"
          className="switch"
          aria-label="连接后自动回到 Vibeface"
          checked={status.preferences.openBusinessOnConnect}
          disabled={busy || hostStarting}
          onChange={(event) => run(() => api.setPreference('openBusinessOnConnect', event.target.checked))}
        />
      </SettingsRow>
      <SettingsRow icon={Laptop} label="清除访问节点选择" description="不会删除节点数据">
        <Button type="button" size="sm" variant="outline" disabled={busy || hostStarting} onClick={() => run(() => api.forgetDataDir())}>清除</Button>
      </SettingsRow>
      <SettingsRow icon={Laptop} label="发行版本">
        <Button type="button" size="sm" variant="ghost" onClick={() => void api.openExternal(RELEASES)}>查看</Button>
      </SettingsRow>
      <SettingsRow icon={Laptop} label="桌面配置目录"><span className="break-all font-mono text-meta">{status.profileDir}</span></SettingsRow>
      <SettingsRow icon={Laptop} label="Core 运行时"><span className="break-all font-mono text-meta">{status.runtime.corePath}</span></SettingsRow>
      <SettingsRow icon={Laptop} label="桌面版本"><span className="text-meta tabular-nums">{status.version}</span></SettingsRow>
    </SettingsGroup> : null}

    <SettingsGroup>
      <Link to="/settings" className="interactive-row block">
        <SettingsRow icon={ChevronRight} label="返回设置"><ChevronRight aria-hidden="true" className="size-4" /></SettingsRow>
      </Link>
    </SettingsGroup>
  </div>
}

function ActionList({ actions, busy, onAction, className }: { actions: Action[]; busy: boolean; onAction: (action: Action) => void; className?: string }) {
  if (actions.length === 0) return null
  return <div className={className ?? 'flex flex-wrap gap-2 px-4 pb-4 pt-1'}>
    {actions.map((action) => (
      <Button
        key={`${action.id}-${action.nodeId ?? 'root'}`}
        type="button"
        size="sm"
        variant={action.primary ? 'default' : 'outline'}
        disabled={busy || action.disabled}
        onClick={() => onAction(action)}
      >
        {busy && action.primary ? <BusyIcon busy /> : null}
        {action.label}
      </Button>
    ))}
  </div>
}
