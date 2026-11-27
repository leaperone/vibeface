import { useEffect, useRef, useState } from 'react'
import { View } from 'react-native'
import { contactCopy } from '@vibeface/client/chat/contact.ts'
import type { BotRecord } from '@vibeface/client/workspace/types.ts'
import { ErrorText } from '@/components/action'
import { Button } from '@/components/arc/button'
import { Text } from '@/components/arc/text'
import { Textarea } from '@/components/arc/textarea'
import { useLive } from '@/data/live'
import { messageOf } from '@vibeface/client/lib/node.ts'

/** Drafts here only become a human-directed Main input after the person presses send. */
export function BotRequestDraft({ bot, initialText, onClose }: { bot: Pick<BotRecord, 'bot_id' | 'conversation_id'>; initialText: string; onClose: () => void }) {
  const shared = useLive()
  const [text, setText] = useState(initialText), [busy, setBusy] = useState(false), [error, setError] = useState('')
  const saving = useRef(false), active = useRef(true)
  useEffect(() => { active.current = true; return () => { active.current = false } }, [])
  const send = async () => {
    if (saving.current || !text.trim() || !shared) return
    saving.current = true; setBusy(true); setError('')
    try {
      await shared.sendMessage(text, [], [], { target: { kind: 'bot_main', bot_id: bot.bot_id }, intent: 'continue', conversation_id: bot.conversation_id })
      if (active.current && shared.isCurrent()) { shared.announce('修改请求已保存在主聊天，等待 Bot 确认。'); onClose() }
    } catch (cause) { if (active.current) setError(messageOf(cause)) }
    finally { saving.current = false; if (active.current) setBusy(false) }
  }
  return <View className="gap-3">
    <Textarea accessibilityLabel="修改请求草稿" value={text} maxLength={65536} editable={!busy} onChangeText={setText} className="min-h-32" autoFocus />
    <Text className="text-sm text-muted">{contactCopy.request}</Text>
    <ErrorText error={error} />
    <View className="flex-row flex-wrap gap-2">
      <Button variant="ghost" disabled={busy} onPress={onClose}><Text>取消</Text></Button>
      <Button disabled={busy || !shared?.canSaveMessages || !text.trim()} accessibilityState={{ busy }} onPress={() => void send()}><Text>{busy ? '保存到聊天…' : '发送给 Bot'}</Text></Button>
    </View>
  </View>
}
