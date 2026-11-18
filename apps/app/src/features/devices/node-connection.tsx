import { SettingsDisclosure,SettingsGroup,toast } from "@/components/arc/product-kit"
import { terms } from "@/lib/glossary"
import { useClipboard } from "@/lib/use-clipboard"
import { cn } from "@/lib/utils"
import { nodeDiagnosticText,nodeDiagnosticTime,nodeStatusLabels,type NodeDiagnostics } from "@vibeface/client/workspace/node-diagnostics.ts"
import type { useNodeChecks } from "@vibeface/client/workspace/use-node-checks.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { ArrowDown,Copy,Globe,RadioTower,RefreshCw,Route } from "lucide-react"

export function NodeStatus({ status }: { status: NodeDiagnostics["status"] }) {
  return <span className="inline-flex shrink-0 items-center gap-1.5 text-sm text-secondary">
    <span aria-hidden="true" className={cn("size-2 rounded-pill", {
      online: "bg-success",
      unreachable: "bg-warning",
      checking: "bg-info",
      stale: "border border-warning",
      unknown: "border border-strong",
      disconnected: "border border-strong",
    }[status])} />
    {nodeStatusLabels[status]}
  </span>
}

const hopIcons = { client: terms.browser.icon, gateway: Globe, relay: RadioTower, node: terms.thisComputer.icon }

export function NodeConnection({ name, diagnostics, checks, canRefresh }: {
  name: string
  diagnostics: NodeDiagnostics
  checks: ReturnType<typeof useNodeChecks>
  canRefresh: boolean
}) {
  const { status, hops, pathLabel, routeNotice, checkedAt, lastSyncedAt, version, system, relay, capabilitiesStale } = diagnostics
  const hasDeviceInfo = Boolean(version || system || relay !== null)
  const canCopy = hops.length > 0 || checkedAt !== null || lastSyncedAt !== null || hasDeviceInfo
  const clipboard = useClipboard(diagnostics.deviceId, {
    onCopied: () => toast("已复制连接详情", { tone: "success" }),
    onFailed: () => toast("复制失败，请重试。", { tone: "warning" }),
  })

  return <SettingsGroup><SettingsDisclosure icon={Route} label="连接详情" value={<span role="status"><NodeStatus status={status} /></span>} open>
    {pathLabel || canRefresh || canCopy ? <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
      {pathLabel ? <p className="min-w-0 text-sm text-secondary [overflow-wrap:anywhere]">{pathLabel}</p> : null}
      {canRefresh || canCopy ? <div className="ms-auto flex flex-wrap gap-2">
        {canRefresh ? <Button type="button" variant="secondary" loading={checks.checking} onClick={checks.refresh}>
          <RefreshCw aria-hidden="true" />重新检查
        </Button> : null}
        {canCopy ? <Button type="button" variant="ghost" loading={clipboard.state.status === "pending"} onClick={() => void clipboard.copy(nodeDiagnosticText(name, diagnostics))}>
          <Copy aria-hidden="true" />复制详情
        </Button> : null}
      </div> : null}
    </div> : null}
    {routeNotice ? <p className="text-sm text-secondary [overflow-wrap:anywhere]">{routeNotice}</p> : null}
    {checks.failed && canRefresh ? <p role="status" className="text-sm text-warning">暂时无法更新连接信息，请重新检查。</p> : null}

    {hops.length ? <div className="grid min-w-0 gap-2">
      <h3 className="text-sm font-medium">连接路径</h3>
      <ol role="list" className="grid min-w-0 list-none gap-1">
        {hops.map((hop, index) => {
          const Icon = hopIcons[hop.kind]
          return <li key={`${hop.kind}-${index}`} className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] gap-x-2">
            <Icon aria-hidden="true" className="mt-0.5 size-4 text-secondary" />
            <div className="min-w-0 [overflow-wrap:anywhere]">
              <p className="flex flex-wrap items-baseline gap-x-2">
                <span className="min-w-0 font-medium">{hop.kind === "relay" ? "中继" : hop.name}</span>
                {hop.kind === "gateway" ? <span className="text-sm text-secondary">网关</span> : null}
                {hop.via ? <span className="min-w-0 text-sm text-secondary">{hop.via}</span> : null}
              </p>
              {hop.address ? <p className="font-mono text-sm text-secondary">{hop.address}</p> : null}
              {hop.kind === "gateway" && hop.deviceId ? <p className="text-sm text-secondary">网关编号 <span className="font-mono">{hop.deviceId.slice(0, 10)}…</span></p> : null}
            </div>
            {index + 1 < hops.length ? <ArrowDown aria-hidden="true" className="col-start-1 mt-1 size-4 text-secondary" /> : null}
          </li>
        })}
      </ol>
    </div> : null}

    {checkedAt !== null || lastSyncedAt !== null || hasDeviceInfo ? <div className="grid gap-2 border-t border-subtle pt-3">
      <dl className="grid min-w-0 gap-x-6 gap-y-2 sm:grid-cols-2">
        {checkedAt !== null ? <div><dt className="text-sm text-secondary">最近检查</dt><dd><time dateTime={new Date(checkedAt).toISOString()}>{nodeDiagnosticTime(checkedAt)}</time></dd></div> : null}
        {lastSyncedAt !== null ? <div><dt className="text-sm text-secondary">最近成功同步</dt><dd><time dateTime={new Date(lastSyncedAt).toISOString()}>{nodeDiagnosticTime(lastSyncedAt)}</time></dd></div> : null}
        {version ? <div><dt className="text-sm text-secondary">Core 版本</dt><dd className="[overflow-wrap:anywhere]">{version}</dd></div> : null}
        {system ? <div><dt className="text-sm text-secondary">系统</dt><dd className="[overflow-wrap:anywhere]">{system}</dd></div> : null}
        {relay !== null ? <div><dt className="text-sm text-secondary">中继服务</dt><dd>{relay ? "已启用" : "未启用"}</dd></div> : null}
      </dl>
      {hasDeviceInfo && capabilitiesStale ? <p className="text-sm text-secondary">设备信息来自上次报告。</p> : null}
    </div> : null}
  </SettingsDisclosure></SettingsGroup>
}
