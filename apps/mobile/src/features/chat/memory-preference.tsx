import { useState } from 'react'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { canRememberMessage,memoryBase,memoryResultLabel,memorySaved } from '@vibeface/client/workspace/memory.ts'
import { newId } from '@vibeface/client/workspace/storage.ts'
import type { MemoryBase,MemoryControlResult,MemoryState } from '@vibeface/client/workspace/types.ts'
import { ErrorText,useAction } from '@/components/action'
import { Button } from '@/components/arc/button'
import { Input } from '@/components/arc/input'
import { Text } from '@/components/arc/text'
import { useLive } from '@/data/live'
import type { ChatMessage } from '@/data/types'

type Draft = { memoryId: string; base: MemoryBase; text: string; result: MemoryControlResult | null; scope: string }

export function MemoryPreference({ message, botId, onClose }: { message: ChatMessage; botId: string; onClose: () => void }) {
  const workspace = useLive()
  const { ready } = useDevice()
  const bot = workspace?.bots.find(item => item.bot_id === botId)
  const scope = JSON.stringify([workspace?.generation, ready?.principal.network_id, ready?.principal.device_id, bot?.conversation_id])
  const [draft, setDraft] = useState<Draft | null>(() => ({ memoryId: newId(), base: memoryBase(null), text: message.text, result: null, scope }))
  const [saved, setSaved] = useState<MemoryState | null>(null)
  const [notice, setNotice] = useState('')
  const action = useAction()
  if (!workspace || !bot || workspace.conversationId !== bot.conversation_id || !canRememberMessage(message, ready?.principal.device_id, botId)) return null
  const start = () => {
    const current = saved && workspace.data?.bot_settings.memory.find(item => item.memory_id === saved.memory_id) || saved
    setDraft({ memoryId: current?.memory_id ?? newId(), base: memoryBase(current?.metadata ?? null), text: current?.text ?? message.text, result: null, scope })
    setNotice(''); action.clear()
  }
  const save = () => {
    if (!draft || draft.scope !== scope) return
    void action.run('preference', async () => {
      const result = await workspace.controlMemory(bot.conversation_id, draft.memoryId, draft.base, draft.text)
      setNotice(memoryResultLabel(result))
      if (memorySaved(result)) { setSaved(result.current); setDraft(null) }
      else setDraft({ ...draft, result })
    })
  }
  return <View className="w-full min-w-0 gap-2">
    {!draft ? <Button variant="ghost" size="sm" className="self-end" disabled={!workspace.writable} onPress={start}><Text>查看或更正偏好</Text></Button> : <View className="min-w-0 gap-3 rounded-panel border border-border bg-surface p-3">
      <Text className="text-sm font-medium">确认给 {bot.name} 的偏好</Text>
      <Input accessibilityLabel="偏好内容" multiline textAlignVertical="top" className="min-h-24" value={draft.text} editable={action.busy === null} onChangeText={text => setDraft({ ...draft, text })} />
      <Text className="text-sm text-muted">只保存你在这里确认的偏好。引用、转贴和附件不会自动成为记忆。最多 500 个字符，保存后由你维护。</Text>
      {draft.result ? <View className="gap-2"><Text accessibilityLiveRegion="polite" className="text-sm">当前版本：{draft.result.current?.text ?? '已删除或尚无记录'}</Text><Button variant="secondary" className="self-start" disabled={action.busy !== null} onPress={() => { setDraft({ ...draft, base: memoryBase(draft.result?.current?.metadata ?? null), result: null }); action.clear(); setNotice('') }}><Text>按当前版本继续编辑</Text></Button></View> : null}
      {draft.scope !== scope ? <Text accessibilityLiveRegion="polite" className="text-sm text-warning">连接已切换，草稿仍在。取消后从原消息重新预览。</Text> : null}
      <View className="flex-row flex-wrap gap-2"><Button disabled={!workspace.writable || action.busy !== null || draft.scope !== scope || !draft.text.trim() || [...draft.text].length > 500} onPress={save}><Text>{action.busy ? '正在查明结果…' : '确认保存偏好'}</Text></Button><Button variant="ghost" disabled={action.busy !== null} onPress={() => { setDraft(null); action.clear(); setNotice(''); onClose() }}><Text>取消编辑</Text></Button></View>
    </View>}
    <ErrorText error={action.error} />
    {action.error ? <Text className="text-sm text-muted">草稿已保留，重试会先查询原操作结果。</Text> : null}
    {notice ? <Text accessibilityLiveRegion="polite" className="text-sm">{notice}</Text> : null}
  </View>
}
