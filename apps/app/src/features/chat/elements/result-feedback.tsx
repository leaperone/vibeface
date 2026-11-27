import { Details,InlineError,useAction } from '@/components/arc/product-kit'
import { Link } from '@tanstack/react-router'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { feedbackLabel,feedbackMessage,feedbackReference,validFeedbackSource,type FeedbackSource } from '@vibeface/client/workspace/feedback.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Textarea } from '@vibeface/uiarc/textarea/textarea.tsx'
import { useRef,useState } from 'react'

type Draft = { source: FeedbackSource; botId: string; botName: string; conversationId: string; generation: number; text: string }
type Saved = Draft & { messageId: string | null }

export function ResultFeedback({ source, conversationId }: { source: FeedbackSource; conversationId: string }) {
  const workspace = useWorkspace()
  const bot = workspace.bots.find(item => item.conversation_id === conversationId)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [saved, setSaved] = useState<Saved | null>(null)
  const sending = useRef(false)
  const action = useAction()
  if (!bot || !validFeedbackSource(source)) return null
  const changed = draft !== null && draft.generation !== workspace.generation
  const save = () => {
    if (!draft || changed || sending.current) return
    sending.current = true
    void action.run('feedback', async () => {
      try {
        let messageId: string | null = null
        const recorded = await workspace.sendMessage(feedbackMessage(draft.source, draft.conversationId, draft.text), [], undefined, { target: { kind: 'bot_main', bot_id: draft.botId }, intent: 'continue', conversation_id: draft.conversationId, onSaved: input => { if (input.conversation_id === draft.conversationId && input.target.kind === 'bot_main' && input.target.bot_id === draft.botId && input.message_id) messageId = input.message_id } })
        if (!recorded) throw new Error('反馈尚未保存，草稿已保留。')
        setSaved({ ...draft, messageId }); setDraft(null)
      } finally { sending.current = false }
    })
  }
  return <div className="grid min-w-0 gap-2">
    {!draft ? <Button type="button" variant="ghost" size="sm" className="justify-self-start" onClick={() => { setDraft({ source: { ...source }, botId: bot.bot_id, botName: bot.name, conversationId, generation: workspace.generation, text: '' }); setSaved(null); action.clear() }}>针对这个结果补充</Button> : <div className="grid min-w-0 gap-3">
      <p className="text-sm font-medium">针对{feedbackLabel(draft.source)}反馈给 {draft.botName}</p>
      <p className="text-sm text-secondary">发到这个 Bot 的主聊天，原任务保持现状；是否继续工作由后续对话确认。</p>
      <Details label="查看引用版本"><p className="whitespace-pre-wrap break-all text-sm">{feedbackReference(draft.source)}</p></Details>
      <Textarea label="成果反馈" rows={3} value={draft.text} maxLength={32000} disabled={action.busy !== null} onChange={event => setDraft({ ...draft, text: event.currentTarget.value })} />
      {changed ? <p role="status" className="text-sm text-warning">连接已切换，草稿仍在。取消后从原成果重新开始。</p> : null}
      <div className="flex flex-wrap gap-2"><Button type="button" disabled={!workspace.canSaveMessages || action.busy !== null || changed || !draft.text.trim()} onClick={save}>{action.busy ? '正在保存反馈…' : '发送反馈到主聊天'}</Button><Button type="button" variant="ghost" disabled={action.busy !== null} onClick={() => { setDraft(null); action.clear() }}>取消反馈</Button></div>
    </div>}
    <InlineError error={action.error} />
    {saved ? <div role="status" className="grid gap-1 text-sm text-secondary"><p>{saved.messageId ? `反馈已保存在 ${saved.botName} 的主聊天，送达状态请查看原消息。` : `反馈已保存，但未取得消息位置；请打开 ${saved.botName} 的主聊天查看是否送达。`}</p><Link to="/b/$botId" params={{ botId: saved.botId }} search={saved.messageId ? { message: saved.messageId } : {}} className="inline-flex min-h-11 w-fit items-center underline underline-offset-4">{saved.messageId ? '查看反馈消息' : '打开主聊天'}</Link>{saved.source.kind === 'task_result' ? <Link to="/tasks/$taskId" params={{ taskId: saved.source.task_id }} className="inline-flex min-h-11 w-fit items-center underline underline-offset-4">返回原任务</Link> : null}</div> : null}
  </div>
}
