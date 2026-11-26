import { InlineError } from '@/components/arc/product-kit'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { contactCopy } from '@vibeface/client/chat/contact.ts'
import type { BotRecord } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Textarea } from '@vibeface/uiarc/textarea/textarea.tsx'
import { useEffect, useRef, useState } from 'react'

/** A human-directed settings request remains editable until its explicit send button saves it to Main. */
export function BotRequestDraft({ bot, initialText, onClose }: { bot: Pick<BotRecord, 'bot_id' | 'conversation_id'>; initialText: string; onClose: () => void }) {
  const workspace = useWorkspace()
  const [text, setText] = useState(initialText), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(null)
  const saving = useRef(false), active = useRef(true)
  useEffect(() => { active.current = true; return () => { active.current = false } }, [])
  const send = async () => {
    if (saving.current || !text.trim()) return
    saving.current = true; setBusy(true); setError(null)
    try {
      await workspace.sendMessage(text, [], [], { target: { kind: 'bot_main', bot_id: bot.bot_id }, intent: 'continue', conversation_id: bot.conversation_id })
      if (active.current && workspace.isCurrent()) { workspace.announce('修改请求已保存在主聊天，等待 Bot 确认。'); onClose() }
    } catch (cause) { if (active.current) setError(cause) }
    finally { saving.current = false; if (active.current) setBusy(false) }
  }
  return <div className="grid min-w-0 gap-3">
    <Textarea id="bot-request-text" label="修改请求草稿" value={text} maxLength={65536} readOnly={busy} onChange={event => setText(event.target.value)} />
    <p className="text-sm text-secondary">{contactCopy.request}</p>
    <InlineError error={error} />
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="ghost" disabled={busy} onClick={onClose}>取消</Button>
      <Button type="button" loading={busy} disabled={!workspace.canSaveMessages || !text.trim()} onClick={() => void send()}>发送给 Bot</Button>
    </div>
  </div>
}
