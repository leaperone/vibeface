import { useEffect } from 'react'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import type { JoinRequest } from '@vibeface/client/lib/node.ts'
import { openJoins } from '@vibeface/client/workspace/inbox.ts'
import { deviceForm, formName } from '@vibeface/client/workspace/labels.ts'
import { deviceIcon } from './device-icon'
import { ErrorText } from '@/components/action'
import { ActionCard, SafetyCode, Skeleton } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Text } from '@/components/ui/text'
import { useRun } from '@/lib/use-run'

/** Requests an admin can still act on. */
export const useOpenJoins = (): JoinRequest[] => openJoins(useDevice())

/**
 * Join requests waiting on an admin, compared by the safety code shown on both screens.
 * `issuing`: this screen just handed out a link, so it starts the comparison without a press.
 * `onAllowed`: the screen shows the added device itself.
 */
export function JoinRequests({ issuing = false, onAllowed }: { issuing?: boolean; onAllowed?(name: string): void }) {
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
  return <View className="gap-3">{joins.map((request) => <JoinCard key={request.join_id} request={request} auto={issuing} onAllowed={onAllowed} />)}</View>
}

// The same request can show on several screens at once; start it only once.
const started = new Set<string>()

/** Starting the comparison claims the request for this admin, so only the screen that issued the link does it on its own. */
function JoinCard({ request, auto, onAllowed }: { request: JoinRequest; auto: boolean; onAllowed?(name: string): void }) {
  const device = useDevice()
  const { busy, error, run } = useRun()
  useEffect(() => {
    if (!auto || request.status !== 'pending' || started.has(request.join_id)) return
    started.add(request.join_id)
    void run('start', () => device.startApproval(request.join_id))
  }, [auto, request.status, request.join_id])
  // Only the admin that started the comparison can allow it (the Node enforces this too).
  const elsewhere = Boolean(request.admin_id && request.admin_id !== device.identity?.deviceId)
  const asking = request.status === 'pending' && !auto
  const allow = (manage: boolean) => run(manage ? 'admin' : 'allow', async () => { await device.approveJoin(request.join_id, manage); onAllowed?.(request.name || '新设备') })
  return <ActionCard icon={deviceIcon({ kind: request.kind, name: request.name, current: false, entry: false })} tone="info" title={`${request.name || '新设备'} 想加入`}
    meta={formName[deviceForm(request.kind, request.name)]} actions={<>
    <Button variant="ghost" disabled={busy !== null} onPress={() => void run('reject', () => device.rejectJoin(request.join_id))}><Text>拒绝</Text></Button>
    {asking ? <Button disabled={busy !== null} onPress={() => void run('start', () => device.startApproval(request.join_id))}><Text>{busy === 'start' ? '处理中' : '显示安全码'}</Text></Button> : <>
      {request.kind === 'client' ? <Button variant="outline" disabled={!request.code || busy !== null || elsewhere} onPress={() => void allow(true)}><Text>{busy === 'admin' ? '处理中' : '允许，设为管理设备'}</Text></Button> : null}
      <Button disabled={!request.code || busy !== null || elsewhere} onPress={() => void allow(false)}><Text>{busy === 'allow' ? '处理中' : '一致，允许'}</Text></Button>
    </>}
  </>}>
    <Text className="text-meta text-muted-foreground">{elsewhere ? '正在另一台管理设备上处理' : asking ? '点“显示安全码”，新设备上会出现同样的码' : request.code ? '和新设备上的安全码比对' : '等新设备显示安全码'}</Text>
    {request.code ? <SafetyCode code={request.code} /> : asking ? null : <Skeleton className="h-8 w-40" />}
    <ErrorText error={error} />
  </ActionCard>
}
