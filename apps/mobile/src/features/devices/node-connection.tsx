import { useIsFocused } from 'expo-router'
import { Check, Copy, Globe, Laptop, RadioTower, RefreshCw, Signal, Smartphone, WifiOff } from 'lucide-react-native'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { describeNode, nodeDiagnosticText, nodeDiagnosticTime, nodeStatusLabels, type NodeDiagnostics } from '@vibeface/client/workspace/node-diagnostics.ts'
import { useNodeChecks } from '@vibeface/client/workspace/use-node-checks.ts'
import { ErrorText } from '@/components/action'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Card } from '@/components/arc/surface'
import { Text } from '@/components/arc/text'
import { CopyableId, Details, SettingsGroup } from '@/components/kit'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import type { Member } from '@/data/types'
import { useClipboardAction } from '@/lib/use-clipboard'
import { cn } from '@/lib/utils'
import { mobilePlatform, self } from '@/platform'

export function useNodeScreen(active: boolean) {
  const device = useDevice()
  const { members } = useWorkspace()
  const live = useLive()
  const focused = useIsFocused()
  const connected = live?.connectionOnline === true
  const report = live?.nodeReport ?? null
  const checks = useNodeChecks({
    report,
    refreshNodes: connected ? live.refreshNodes : undefined,
    active: focused && active,
    foreground: mobilePlatform.foreground,
  })
  const names = new Map(members.map(member => [member.device_id, member.name]))

  return {
    checks, connected,
    describe: (member: Member) => describeNode({
      target: { deviceId: member.device_id, name: member.name, entry: member.entry },
      ready: device.ready, connected, report, fresh: checks.fresh, checking: checks.checking,
      clientLabel: self, nameFor: id => names.get(id),
    }),
  }
}

export function NodeStatus({ status }: { status: NodeDiagnostics['status'] }) {
  const online = status === 'online'
  const unavailable = status === 'unreachable' || status === 'syncFailed' || status === 'disconnected'
  const color = online ? 'text-success' : unavailable ? 'text-warning' : 'text-muted'
  return <View className="min-w-0 flex-row items-center gap-1.5">
    <Icon as={unavailable ? WifiOff : Signal} className={cn('size-4', color)} />
    <Text className={cn('shrink text-sm', color)}>{nodeStatusLabels[status]}</Text>
  </View>
}

const hopIcons = { client: Smartphone, gateway: Globe, relay: RadioTower, node: Laptop }

export function NodeConnection({ member, diagnostics, checks, connected, reconnect }: {
  member: Member
  diagnostics: NodeDiagnostics
  checks: ReturnType<typeof useNodeChecks>
  connected: boolean
  reconnect?: () => void
}) {
  const { hops, pathLabel, routeNotice, returnPath, checkedAt, lastSyncedAt, version, system, relay, capabilitiesStale } = diagnostics
  const copy = useClipboardAction(diagnostics.deviceId)
  return <>
    <Card>
      <Details label="连接路径" open>
        {pathLabel ? <Text className="font-medium">{pathLabel}</Text> : null}
        {hops.map((hop, index) => <View key={`${hop.kind}-${index}`} className="min-w-0 flex-row items-stretch gap-3">
          <View className="w-5 items-center" accessible={false}>
            <Icon as={hopIcons[hop.kind]} className="mt-1 size-4 text-secondary" />
            {index + 1 < hops.length ? <View className="mt-1 min-h-4 w-px flex-1 bg-border" /> : null}
          </View>
          <View className="min-w-0 flex-1 gap-0.5">
            {hop.via ? <Text className="text-sm text-muted">{hop.via}</Text> : null}
            <Text className="font-medium">{hop.kind === 'relay' ? '中继' : hop.name}</Text>
            {hop.address ? <Text selectable className="font-mono text-sm text-secondary">{hop.address}</Text> : null}
            {hop.kind === 'gateway' && hop.deviceId ? <Text className="text-sm text-muted">网关 · {hop.deviceId.slice(0, 12)}</Text> : null}
          </View>
        </View>)}
        {routeNotice ? <Text className="text-sm text-secondary">{routeNotice}</Text> : null}
        {returnPath ? <View className="gap-1 border-t border-subtle pt-3">
          <Text className="text-sm font-medium">回程路径</Text>
          <Text className="text-sm">{returnPath.from} → {returnPath.to} · {returnPath.path.kind === 'relay' ? '经中继' : '直连'}</Text>
          {returnPath.path.kind === 'relay' ? <Text selectable className="font-mono text-sm text-secondary">{returnPath.path.url}</Text> : null}
          {!member.entry ? <Text className="text-sm text-muted">回程检查 {nodeDiagnosticTime(returnPath.checkedAt)}</Text> : null}
        </View> : null}
        {checkedAt !== null ? <Text className="text-sm text-muted">最近检查 {nodeDiagnosticTime(checkedAt)}</Text> : null}
        {!member.entry && (checkedAt !== null || lastSyncedAt !== null) ? <Text className="text-sm text-muted">
          {lastSyncedAt === null ? '暂无成功同步记录' : `最近成功同步 ${nodeDiagnosticTime(lastSyncedAt)}`}
        </Text> : null}
      </Details>
      <View className="flex-row flex-wrap gap-2">
        {connected ? <Button variant="secondary" disabled={checks.checking} accessibilityState={{ busy: checks.checking }} onPress={checks.refresh}>
          <Icon as={RefreshCw} /><Text>{checks.checking ? '检查中…' : '重新检查'}</Text>
        </Button> : reconnect ? <Button variant="secondary" onPress={reconnect}><Icon as={RefreshCw} /><Text>重新连接</Text></Button> : null}
        <Button variant="ghost" disabled={copy.busy} onPress={() => void copy.copy(nodeDiagnosticText(member.name, diagnostics))}>
          <Icon as={copy.copied ? Check : Copy} /><Text>{copy.copied ? '已复制' : '复制诊断'}</Text>
        </Button>
      </View>
      {checks.failed ? <ErrorText error="连接信息暂时无法更新，请重新检查。" /> : null}
      <ErrorText error={copy.error} />
    </Card>
    <SettingsGroup title="节点信息">
      <NodeFact label="Core 版本" value={version ?? '待节点上报'} />
      <NodeFact label="系统" value={system ?? '待节点上报'} />
      <NodeFact label="转发服务" value={relay === null ? '状态未知' : relay ? '已启用' : '未启用'} />
      {capabilitiesStale ? <Text className="py-2 text-sm text-muted">设备信息来自上次报告。</Text> : null}
      <View className="gap-2 pt-3">
        <Text className="text-sm text-secondary">节点编号</Text>
        <CopyableId value={member.device_id} label="节点编号" />
      </View>
    </SettingsGroup>
  </>
}

function NodeFact({ label, value }: { label: string; value: string }) {
  return <View className="min-h-12 flex-row flex-wrap items-baseline justify-between gap-x-4 gap-y-1 py-3">
    <Text className="text-sm text-secondary">{label}</Text>
    <Text className="shrink">{value}</Text>
  </View>
}
