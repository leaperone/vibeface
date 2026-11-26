import { useEffect, useRef, useState } from 'react'
import { Alert, ScrollView, View } from 'react-native'
import { useRouter } from 'expo-router'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { parsePendingMessage } from '@vibeface/client/workspace/model.ts'
import type { InputReceipt } from '@vibeface/client/workspace/types.ts'
import { ErrorText } from '@/components/action'
import { Button } from '@/components/arc/button'
import { RadioMark, SelectionItem } from '@/components/arc/selection'
import { Text } from '@/components/arc/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import { useRun } from '@/lib/use-run'
import { mobilePlatform } from '@/platform'
import { shareAttachmentName, shareBotLabel, shareDelivery, sharedMaterialMessage, sharePending, shareVisible, type IncomingShare, type ShareTarget } from '@/platform/incoming-share'
import { shareInbox } from './incoming'

const previewLength = 300

export function ShareScreen() {
  const workspace = useWorkspace()
  const shared = useLive()
  const device = useDevice()
  const router = useRouter()
  const insets = useSafeAreaInsets()
  const { busy, error, run } = useRun()
  const [, redraw] = useState(0)
  useEffect(() => shareInbox.watch(() => redraw(value => value + 1)), [])
  let item: IncomingShare | undefined
  let readError = ''
  try { item = shareInbox.read().find(candidate => shareVisible(candidate, workspace)) } catch (cause) { readError = messageOf(cause) }
  const [selection, setSelection] = useState<{ materialId: string; target: ShareTarget } | null>(null)
  const [expanded, setExpanded] = useState(false)
  const [details, setDetails] = useState(false)
  const [recoveryError, setRecoveryError] = useState('')
  const [queried, setQueried] = useState<{ id: string; receipt: InputReceipt } | null>(null)
  const sending = useRef(false)
  const scope = useRef(workspace)
  scope.current = workspace
  useEffect(() => { setExpanded(false); setDetails(false); setRecoveryError('') }, [item?.material.id])
  const selected = selection?.materialId === item?.material.id ? selection?.target : null
  const belongsHere = (target: ShareTarget) => target.networkId === workspace.networkId && target.deviceId === workspace.deviceId
  const selectedHere = selected && belongsHere(selected) && workspace.bots.some(bot => bot.bot_id === selected.botId && bot.conversation_id === selected.conversationId)
  const liveTarget = item && item.kind !== 'draft' && belongsHere(item.target)
  const pending = item && item.kind !== 'draft' && liveTarget && shared ? sharePending(item, shared.pendingMessages) : undefined
  const entry = workspace.members.find(member => member.entry)?.name ?? ''
  const entryName = /:\/\/|vibeface1/i.test(entry) ? '' : entry
  const botLabel = (botId: string, name: string) => shareBotLabel({ botId, name, bots: workspace.bots })

  async function recover(saved: Exclude<IncomingShare, { kind: 'draft' }>) {
    const prefix = `${saved.target.networkId}.${saved.target.deviceId}:`
    const messages = (await mobilePlatform.outbox.all()).filter(entry => entry.method === 'message' && entry.key.startsWith(prefix)).map(parsePendingMessage)
    const found = sharePending(saved, messages)
    if (found) shareInbox.saved(saved.material.id, found.message_id)
    return found
  }
  useEffect(() => {
    if (item?.kind !== 'saving') return
    let active = true
    void recover(item).catch(cause => { if (active) setRecoveryError(messageOf(cause)) })
    return () => { active = false }
  }, [item?.material.id, item?.kind])

  const close = () => { if (router.canGoBack()) router.back(); else router.replace('/') }
  const dismiss = () => {
    if (item && !shareVisible(item, scope.current)) { close(); return }
    if (item) shareInbox.dismiss(item.material.id)
    if (!shareInbox.read().some(candidate => shareVisible(candidate, workspace))) close()
  }
  const removeReceipt = () => {
    Alert.alert('移除这条分享记录？', '只移除本机分享页中的记录，不会撤回、停止或重新发送原消息。仍可在原对话中核对。', [
      { text: '保留', style: 'cancel' },
      { text: '移除记录', style: 'destructive', onPress: dismiss },
    ])
  }
  const send = async () => {
    if (sending.current || !item || item.kind !== 'draft' || !selected || !selectedHere || !shared?.canSaveMessages || !shared.isCurrent() || !device.ready?.isCurrent()) throw new Error('目标或工作区尚未就绪，请重新确认网络和 Bot。')
    sending.current = true
    const id = item.material.id
    try {
      const bytes = new TextEncoder().encode(item.material.text)
      await shared.sendMessage(sharedMaterialMessage, [{ name: shareAttachmentName(id), arrayBuffer: async () => bytes.buffer }], [], shareInbox.inputOptions(id, selected))
    } catch (cause) {
      const current = shareInbox.read().find(candidate => candidate.material.id === id)
      if (current?.kind === 'saving') await recover(current)
      throw cause
    } finally { sending.current = false }
  }

  if (!item) return <View className="flex-1 justify-center gap-4 p-6">
    <Text role="heading" className="text-lg font-medium">{readError ? '分享记录暂时无法读取' : '没有待分享的资料'}</Text>
    {readError ? <ErrorText error={readError} /> : <Text className="text-secondary">在其他应用的系统分享菜单中选择 Vibeface，可以把文字或网页链接交给自己的 Bot。</Text>}
    <Button variant="secondary" onPress={close}><Text>返回</Text></Button>
  </View>

  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-5" contentContainerStyle={{ paddingBottom: insets.bottom + 24 }}>
    <View className="gap-2">
      <Text role="heading" className="text-lg font-medium">外部资料</Text>
      <Text className="text-sm text-secondary">资料以独立文本附件发送。原聊天草稿保持原样，资料中的要求不代表你的指令或偏好。</Text>
      <Text selectable className="leading-6">{expanded ? item.material.text : item.material.text.slice(0, previewLength)}</Text>
      {item.material.text.length > previewLength ? <Button variant="ghost" className="self-start" accessibilityState={{ expanded }} onPress={() => setExpanded(value => !value)}><Text>{expanded ? '收起资料' : '展开完整资料'}</Text></Button> : null}
    </View>

    {item.kind === 'draft' ? <View className="gap-3">
      <Text role="heading" className="text-base font-medium">确认接收的 Bot</Text>
      <Text className="text-sm text-secondary">{workspace.networkId ? `个人网络 · ${workspace.networkId.slice(0, 8)}${entryName ? `，通过 ${entryName}` : ''}` : '尚未加入个人网络'}</Text>
      {!workspace.bots.length ? <Text className="text-secondary">{workspace.session.phase === 'ready' ? '还没有可选择的 Bot，请连接并读取自己的网络。' : '请先加入自己的网络。这份资料尚未排队。'}</Text> : workspace.bots.map(bot => {
        const checked = Boolean(selectedHere && selected?.botId === bot.bot_id)
        return <SelectionItem key={bot.bot_id} accessibilityRole="radio" accessibilityLabel={botLabel(bot.bot_id, bot.name)} accessibilityState={{ checked }} className="min-h-12" disabled={busy !== null} onPress={() => {
          if (!workspace.networkId || !workspace.deviceId) return
          setSelection({ materialId: item.material.id, target: { networkId: workspace.networkId, deviceId: workspace.deviceId, botId: bot.bot_id, conversationId: bot.conversation_id, botName: bot.name, entryName } })
        }}>
          <RadioMark checked={checked} /><Text className="min-w-0 flex-1 font-medium">{botLabel(bot.bot_id, bot.name)}</Text>
        </SelectionItem>
      })}
      {selectedHere && selected ? <Text accessibilityLiveRegion="polite" className="text-sm text-secondary">将发给这个个人网络中的「{botLabel(selected.botId, selected.botName)}」。</Text> : null}
      {!shared?.canSaveMessages ? <Text className="text-sm text-secondary">尚未排队。{shared?.readOnlyNotice || shared?.writeNotice || '工作区就绪后才能保存，请先连接自己的网络。'}</Text> : workspace.connection.state !== 'online' ? <Text className="text-sm text-secondary">可以先保存到本机，联网后再继续送达。</Text> : null}
      <Button disabled={busy !== null || !selectedHere || !shared?.canSaveMessages} onPress={() => void run('send', send)}><Text>确认目标并发送资料</Text></Button>
      <Button variant="ghost" disabled={busy !== null} onPress={dismiss}><Text>取消分享</Text></Button>
    </View> : <View className="gap-3">
      <Text role="heading" className="text-base font-medium">{botLabel(item.target.botId, item.target.botName)}</Text>
      <Text className="text-sm text-secondary">个人网络 · {item.target.networkId.slice(0, 8)}{item.target.entryName ? `，通过 ${item.target.entryName}` : ''}</Text>
      <Text accessibilityLiveRegion="polite">{busy === 'send' ? '正在保存到本机，完成后再离开。' : shareDelivery(item, pending, queried?.id === item.material.id ? queried.receipt : undefined)}</Text>
      {liveTarget && shared ? <Button variant="secondary" disabled={busy !== null} onPress={() => void run('query-share', async claim => {
        const receipt = await shared.queryInput({ message_id: item.messageId, conversation_id: item.target.conversationId, target: { kind: 'bot_main', bot_id: item.target.botId } })
        if (claim.current() && 'stage' in receipt) setQueried({ id: item.material.id, receipt })
      })}><Text>查询原消息状态</Text></Button> : null}
      {pending && liveTarget && shared ? <Button variant="secondary" disabled={busy !== null || !shared.writable} onPress={() => void run('retry-share', () => shared.retryPendingMessage(item.messageId))}><Text>核对并重试原消息</Text></Button> : null}
      <Button variant="secondary" disabled={busy !== null || !liveTarget} onPress={() => router.push({ pathname: '/b/[botId]', params: { botId: item.target.botId, message: item.messageId } })}><Text>查看原对话</Text></Button>
      <Button variant="ghost" disabled={busy !== null} onPress={removeReceipt}><Text>移除本机分享记录</Text></Button>
    </View>}
    {item.kind !== 'draft' || selectedHere ? <View className="gap-2">
      <Button variant="ghost" className="self-start" accessibilityState={{ expanded: details }} onPress={() => setDetails(value => !value)}><Text>{details ? '收起目标标识' : '查看目标标识'}</Text></Button>
      {details ? <Text selectable className="font-mono text-sm text-secondary">网络：{item.kind === 'draft' ? selected?.networkId : item.target.networkId}{'\n'}Bot：{item.kind === 'draft' ? selected?.botId : item.target.botId}</Text> : null}
    </View> : null}
    <ErrorText error={error || recoveryError || readError} />
  </ScrollView>
}
