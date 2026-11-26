import { Link } from 'expo-router'
import { useRef,useState } from 'react'
import { View } from 'react-native'
import { feedbackLabel,feedbackMessage,feedbackReference,validFeedbackSource,type FeedbackSource } from '@vibeface/client/workspace/feedback.ts'
import { ErrorText,useAction } from '@/components/action'
import { Button } from '@/components/arc/button'
import { Input } from '@/components/arc/input'
import { Text } from '@/components/arc/text'
import { Details } from '@/components/kit'
import { useLive } from '@/data/live'

type Draft = { source: FeedbackSource; botId: string; botName: string; conversationId: string; generation: number; text: string }

export function ResultFeedback({ source, conversationId }: { source: FeedbackSource; conversationId: string }) {
  const workspace = useLive()
  const bot = workspace?.bots.find(item => item.conversation_id === conversationId)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [saved, setSaved] = useState<Draft | null>(null)
  const sending = useRef(false)
  const action = useAction()
  if (!workspace || !bot || !validFeedbackSource(source)) return null
  const changed = draft !== null && draft.generation !== workspace.generation
  const save = () => {
    if (!draft || changed || sending.current) return
    sending.current = true
    void action.run('feedback', async () => {
      try {
        const recorded = await workspace.sendMessage(feedbackMessage(draft.source, draft.conversationId, draft.text), [], undefined, { target: { kind: 'bot_main', bot_id: draft.botId }, intent: 'continue', conversation_id: draft.conversationId })
        if (!recorded) throw new Error('反馈尚未保存，草稿已保留。')
        setSaved(draft); setDraft(null)
      } finally { sending.current = false }
    })
  }
  return <View className="min-w-0 gap-2">
    {!draft ? <Button variant="ghost" size="sm" className="self-start" onPress={() => { setDraft({ source: { ...source }, botId: bot.bot_id, botName: bot.name, conversationId, generation: workspace.generation, text: '' }); setSaved(null); action.clear() }}><Text>针对这个结果补充</Text></Button> : <View className="min-w-0 gap-3">
      <Text className="text-sm font-medium">针对{feedbackLabel(draft.source)}反馈给 {draft.botName}</Text>
      <Text className="text-sm text-muted">发到这个 Bot 的主聊天，原任务保持现状；是否继续工作由后续对话确认。</Text>
      <Details label="查看引用版本"><Text selectable className="text-sm">{feedbackReference(draft.source)}</Text></Details>
      <Input accessibilityLabel="成果反馈" multiline textAlignVertical="top" className="min-h-24" value={draft.text} maxLength={32000} editable={action.busy === null} onChangeText={text => setDraft({ ...draft, text })} />
      {changed ? <Text accessibilityLiveRegion="polite" className="text-sm text-warning">连接已切换，草稿仍在。取消后从原成果重新开始。</Text> : null}
      <View className="flex-row flex-wrap gap-2"><Button disabled={!workspace.canSaveMessages || action.busy !== null || changed || !draft.text.trim()} onPress={save}><Text>{action.busy ? '正在保存反馈…' : '发送反馈到主聊天'}</Text></Button><Button variant="ghost" disabled={action.busy !== null} onPress={() => { setDraft(null); action.clear() }}><Text>取消反馈</Text></Button></View>
    </View>}
    <ErrorText error={action.error} />
    {saved ? <View accessibilityLiveRegion="polite" className="gap-1"><Text className="text-sm text-muted">反馈已保存在 {saved.botName} 的主聊天，送达状态请查看消息。</Text><Link href={`/b/${saved.botId}`} asChild><Button variant="ghost" className="self-start"><Text>查看反馈消息</Text></Button></Link>{saved.source.kind === 'task_result' ? <Link href={`/task/${saved.source.task_id}`} asChild><Button variant="ghost" className="self-start"><Text>返回原任务</Text></Button></Link> : null}</View> : null}
  </View>
}
