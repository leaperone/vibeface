import * as Haptics from 'expo-haptics'
import { useEffect, useState } from 'react'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { deviceCopy } from '@vibeface/client/identity/copy.ts'
import type { JoinRequest } from '@vibeface/client/lib/node.ts'
import { openJoins } from '@vibeface/client/workspace/inbox.ts'
import { deviceForm, formName } from '@vibeface/client/workspace/labels.ts'
import { deviceIcon } from './device-icon'
import { ErrorText } from '@/components/action'
import { ActionCard, SafetyCode, Skeleton } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Switch } from '@/components/arc/switch'
import { Text } from '@/components/arc/text'
import { useRun } from '@/lib/use-run'

/** A device this screen just allowed in. */
export type Added = { id: string; kind: 'node' | 'client'; name: string; manage: boolean }
type OnAllowed = (added: Added, inviteId: string) => void

/** Requests an admin can still act on. */
export const useOpenJoins = (): JoinRequest[] => openJoins(useDevice())

/**
 * Join requests waiting on an admin, compared by the safety code shown on both screens.
 * `inviteId`: only a device that used this screen's link starts the comparison without a press.
 * `onAllowed`: the screen shows the added device itself.
 */
export function JoinRequests({ inviteId, onAllowed }: { inviteId?: string; onAllowed?: OnAllowed }) {
  const device = useDevice()
  const joins = useOpenJoins()
  // The code appears once the new device reveals its nonce; poll until every open request shows one.
  const waiting = joins.some((request) => !request.code)
  useEffect(() => {
    if (!waiting) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 2000)
    return () => clearInterval(timer)
  }, [waiting, device.ready])
  if (!joins.length) return null
  return <View className="gap-3">{joins.map((request) => <JoinCard key={request.join_id} request={request} auto={request.invite_id === inviteId} onAllowed={onAllowed} />)}</View>
}

// The same request can show on several screens at once; start it only once.
const started = new Set<string>()

/** Starting the comparison claims the request for this admin, so only the screen that issued the link does it on its own. */
function JoinCard({ request, auto, onAllowed }: { request: JoinRequest; auto: boolean; onAllowed?: OnAllowed }) {
  const device = useDevice()
  const { busy, error, run } = useRun()
  const asking = request.status === 'pending'
  const start = () => {
    started.add(request.join_id)
    void run('start', () => device.startApproval(request.join_id))
  }
  useEffect(() => {
    if (auto && asking && !started.has(request.join_id)) start()
  }, [auto, asking, request.join_id])
  // Only the admin that started the comparison can allow it (the Node enforces this too).
  const elsewhere = Boolean(request.admin_id && request.admin_id !== device.identity?.deviceId)
  // As on the web: one 一致，允许 after the codes match, with 设为管理设备 as a switch beside the request.
  const [manage, setManage] = useState(false)
  const allow = () => run('allow', async () => { const asManager = request.kind === 'client' && manage; await device.approveJoin(request.join_id, asManager); void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success); onAllowed?.({ id: request.device_id, kind: request.kind, name: request.name || '新设备', manage: asManager }, request.invite_id) })
  return <ActionCard kind="approval" icon={deviceIcon({ kind: request.kind, name: request.name, current: false })} tone="info" title={`${request.name || '新设备'} 想加入`}
    meta={formName[deviceForm(request.kind, request.name)]} actions={<>
    <Button variant="ghost" disabled={busy !== null} onPress={() => void run('reject', () => device.rejectJoin(request.join_id))}><Text>拒绝</Text></Button>
    {asking ? <Button disabled={busy !== null || elsewhere} onPress={start}><Text>{busy === 'start' ? '处理中' : '显示安全码'}</Text></Button> : <>
      <Button disabled={!request.code || busy !== null || elsewhere} onPress={() => void allow()}><Text>{busy === 'allow' ? '处理中' : '一致，允许'}</Text></Button>
    </>}
  </>}>
    <Text role="status" accessibilityLiveRegion="polite" className="text-sm text-muted">{elsewhere ? '正在另一台管理设备上处理' : asking && busy !== 'start' ? deviceCopy.startCode : request.code ? deviceCopy.compareCode : deviceCopy.waitForCode}</Text>
    {elsewhere ? null : request.code ? <SafetyCode code={request.code} /> : !asking || busy === 'start' ? <Skeleton className="h-8 w-40" /> : null}
    {request.kind === 'client' && !asking && !elsewhere ? <View className="flex-row items-center gap-3">
      <View className="min-w-0 flex-1">
        <Text>设为管理设备</Text>
        <Text className="text-sm text-muted">{deviceCopy.manageHint}</Text>
      </View>
      <Switch accessibilityLabel="设为管理设备" accessibilityHint={deviceCopy.manageHint} value={manage} disabled={busy !== null} onValueChange={setManage} />
    </View> : null}
    <ErrorText error={error} />
  </ActionCard>
}
