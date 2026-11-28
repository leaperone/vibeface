import { InlineError,useAction } from '@/components/arc/product-kit'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { canRememberMessage,memoryBase,memoryResultLabel,memorySaved } from '@vibeface/client/workspace/memory.ts'
import { newId } from '@vibeface/client/workspace/storage.ts'
import type { MemoryBase,MemoryControlResult,MemoryState } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Textarea } from '@vibeface/uiarc/textarea/textarea.tsx'
import { Popover,PopoverClose,PopoverContent,PopoverTrigger } from '@vibeface/uiarc/popover/popover.tsx'
import { MoreHorizontal } from 'lucide-react'
import { useState } from 'react'
import type { ChatMessage } from '@vibeface/client/chat/types.ts'

type Draft = { memoryId: string; base: MemoryBase; text: string; result: MemoryControlResult | null; scope: string }

export function MemoryPreference({ message, conversationId }: { message: ChatMessage; conversationId: string }) {
  const workspace = useWorkspace()
  const { ready } = useDevice()
  const bot = workspace.bots.find(item => item.conversation_id === conversationId)
  const scope = JSON.stringify([workspace.generation, ready?.principal.network_id, ready?.principal.device_id, conversationId])
  const [draft, setDraft] = useState<Draft | null>(null)
  const [saved, setSaved] = useState<MemoryState | null>(null)
  const [notice, setNotice] = useState('')
  const action = useAction()
  if (!bot || workspace.conversationId !== conversationId || !canRememberMessage(message, ready?.principal.device_id, bot.bot_id)) return null
  const start = () => {
    const current = saved && workspace.data?.bot_settings.memory.find(item => item.memory_id === saved.memory_id) || saved
    setDraft({ memoryId: current?.memory_id ?? newId(), base: memoryBase(current?.metadata ?? null), text: current?.text ?? message.text, result: null, scope })
    setNotice(''); action.clear()
  }
  const save = () => {
    if (!draft || draft.scope !== scope) return
    void action.run('preference', async () => {
      const result = await workspace.controlMemory(conversationId, draft.memoryId, draft.base, draft.text)
      setNotice(memoryResultLabel(result))
      if (memorySaved(result)) { setSaved(result.current); setDraft(null) }
      else setDraft({ ...draft, result })
    })
  }
  return <div className="grid w-full min-w-0 gap-2 text-start">
    {!draft ? saved ? <Button type="button" variant="ghost" size="sm" className="justify-self-end" disabled={!workspace.writable} onClick={start}>查看或更正偏好</Button> : <Popover><PopoverTrigger asChild><Button type="button" variant="ghost" size="sm" className="justify-self-end" aria-label="更多消息操作" title="消息操作"><MoreHorizontal aria-hidden="true" /></Button></PopoverTrigger><PopoverContent align="end" className="grid gap-1"><PopoverClose asChild><Button type="button" variant="ghost" disabled={!workspace.writable} onClick={start}>记为这个 Bot 的偏好</Button></PopoverClose></PopoverContent></Popover> : <div className="grid min-w-0 gap-3 rounded-panel border bg-surface p-3">
      <p className="text-sm font-medium">确认给 {bot.name} 的偏好</p>
      <Textarea label="偏好内容" rows={3} value={draft.text} disabled={action.busy !== null} onChange={event => setDraft({ ...draft, text: event.currentTarget.value })} />
      <p className="text-sm text-secondary">只保存你在这里确认的偏好。引用、转贴和附件不会自动成为记忆。最多 500 个字符，保存后由你维护。</p>
      {draft.result ? <div className="grid gap-2" role="status"><p className="whitespace-pre-wrap break-words text-sm">当前版本：{draft.result.current?.text ?? '已删除或尚无记录'}</p><Button type="button" variant="secondary" className="justify-self-start" disabled={action.busy !== null} onClick={() => { setDraft({ ...draft, base: memoryBase(draft.result?.current?.metadata ?? null), result: null }); action.clear(); setNotice('') }}>按当前版本继续编辑</Button></div> : null}
      {draft.scope !== scope ? <p role="status" className="text-sm text-warning">连接已切换，草稿仍在。取消后从原消息重新预览。</p> : null}
      <div className="flex flex-wrap gap-2"><Button type="button" disabled={!workspace.writable || action.busy !== null || draft.scope !== scope || !draft.text.trim() || [...draft.text].length > 500} onClick={save}>{action.busy ? '正在查明结果…' : '确认保存偏好'}</Button><Button type="button" variant="ghost" disabled={action.busy !== null} onClick={() => { setDraft(null); action.clear(); setNotice('') }}>取消编辑</Button></div>
    </div>}
    <InlineError error={action.error} />
    {action.error ? <p className="text-sm text-secondary">草稿已保留，重试会先查询原操作结果。</p> : null}
    {notice ? <p role="status" className="text-sm">{notice}</p> : null}
  </div>
}
