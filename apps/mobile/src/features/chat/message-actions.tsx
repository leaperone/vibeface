import { useEffect, useRef } from 'react'
import { MoreHorizontal } from 'lucide-react-native'
import { ActionSheetIOS, Alert, Platform, Share, View } from 'react-native'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import type { ChatMessage } from '@/data/types'
import { useClipboardAction } from '@/lib/use-clipboard'

export type MessageActionsProps = {
  onCopy?: (message: ChatMessage) => void | Promise<unknown>
  onShare?: (message: ChatMessage) => void | Promise<unknown>
  onQuote?: (message: ChatMessage) => void | Promise<unknown>
}
export function MessageActions({ message, onCopy, onShare, onQuote }: MessageActionsProps & { message: ChatMessage }) {
  const version = useRef(0), mounted = useRef(true)
  useEffect(() => { mounted.current = true; version.current++; return () => { mounted.current = false; version.current++ } }, [message])
  const copy = useClipboardAction(message)
  const text = [message.text, ...(message.attachments ?? []).map(file => `附件：${file.name}`)].filter(Boolean).join('\n')
  const run = async (action: () => void | Promise<unknown>) => {
    try { await action() } catch { Alert.alert('操作未完成', '请重试。') }
  }
  const choose = () => {
    const owner = version.current
    const current = () => mounted.current && owner === version.current
    const options = [
      { text: '复制整条消息', action: () => run(async () => { if (onCopy) await onCopy(message); else await copy.copy(text) }) },
      { text: '分享消息', action: () => run(async () => { if (onShare) await onShare(message); else await Share.share({ message: text }) }) },
      ...(onQuote ? [{ text: '引用消息', action: () => run(() => onQuote(message)) }] : []),
    ]
    if (Platform.OS === 'ios') ActionSheetIOS.showActionSheetWithOptions({ options: [...options.map(item => item.text), '取消'], cancelButtonIndex: options.length }, index => { if (current()) void options[index]?.action() })
    else Alert.alert('消息操作', undefined, options.map(item => ({ text: item.text, onPress: () => { if (current()) void item.action() } })), { cancelable: true })
  }
  return <View className="flex-row items-center gap-1"><Button variant="ghost" iconOnly size="sm" accessibilityLabel="消息操作：复制、分享或引用" disabled={copy.busy || !text} onPress={choose}><Icon as={MoreHorizontal} /></Button>
    {copy.copied ? <Text accessibilityLiveRegion="polite" className="text-sm text-muted">已复制</Text> : null}
    {copy.error ? <Text accessibilityLiveRegion="polite" className="shrink text-sm text-danger">{copy.error}</Text> : null}
  </View>
}
