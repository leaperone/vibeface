import { Stack, useLocalSearchParams, useRouter } from 'expo-router'
import { LoaderCircle } from 'lucide-react-native'
import { useState, useSyncExternalStore } from 'react'
import { View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { lastFailure, problemReport, subscribeFailures } from '@vibeface/client/lib/error-ref.ts'
import { sendLogs } from '@vibeface/client/lib/web-log.ts'
import { ErrorText, useAction } from '@/components/action'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Switch } from '@/components/arc/switch'
import { Text } from '@/components/arc/text'
import { Textarea } from '@/components/arc/textarea'
import { useLive } from '@/data/live'

const noPrimary = '还没有主 Bot。先在 Bot 信息里设一个。'

/**
 * 报告问题, as on the web: the description, with the conversation it came from and the latest error number when kept,
 * goes to the primary bot as an ordinary message; the app then opens that bot to follow its answer.
 */
export default function Report() {
  const shared = useLive()
  const router = useRouter()
  const params = useLocalSearchParams<{ conversation?: string }>()
  const failure = useSyncExternalStore(subscribeFailures, lastFailure, lastFailure)
  const [text, setText] = useState('')
  const [withConversation, setWithConversation] = useState(true)
  const [withRef, setWithRef] = useState(true)
  const { busy, error, run } = useAction()
  if (!shared) return null
  const primary = shared.bots.find((bot) => bot.conversation_id === shared.primary)
  const conversation = params.conversation || failure?.conversation_id
  const conversationName = shared.bots.find((bot) => bot.conversation_id === conversation)?.name
  const send = () => run('send', async () => {
    if (!primary) throw new Error(noPrimary)
    // The bot reads the device's log, so the phone's kept records go first, waiting at most a few seconds.
    await Promise.race([sendLogs(), new Promise(resolve => setTimeout(resolve, 3000))])
    const message = problemReport({ description: text, conversation_id: withConversation ? conversation : undefined, ref: withRef ? failure?.ref : undefined })
    await shared.sendMessage(message, [], [], { conversation_id: primary.conversation_id, target: { kind: 'bot_main', bot_id: primary.bot_id } })
    router.replace(`/b/${primary.bot_id}`)
  })
  const disabled = busy !== null || !shared.writable || !primary || !text.trim()
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12">
    {/* In the header, so the keyboard never covers it. */}
    <Stack.Screen options={{ headerRight: () => <Button variant="ghost" className="px-3" disabled={disabled} onPress={() => void send()}>
      {busy ? <Icon as={LoaderCircle} /> : null}<Text className="font-medium">发送</Text>
    </Button> }} />
    <View className="gap-2">
      <Text className="text-sm font-medium text-muted">遇到了什么问题</Text>
      <Textarea accessibilityLabel="遇到了什么问题" autoFocus className="min-h-28" maxLength={4000} value={text} editable={busy === null} onChangeText={setText} />
    </View>
    {conversation || failure ? <View className="gap-1">
      {conversation ? <View className="min-h-11 flex-row items-center gap-3">
        <Text className="min-w-0 flex-1">{conversationName ? `附带会话「${conversationName}」` : '附带当前会话'}</Text>
        <Switch accessibilityLabel={conversationName ? `附带会话「${conversationName}」` : '附带当前会话'} value={withConversation} onValueChange={setWithConversation} />
      </View> : null}
      {failure ? <View className="min-h-11 flex-row items-center gap-3">
        <Text className="min-w-0 flex-1">附带错误编号 <Text className="font-mono">#{failure.ref}</Text></Text>
        <Switch accessibilityLabel={`附带错误编号 ${failure.ref}`} value={withRef} onValueChange={setWithRef} />
      </View> : null}
    </View> : null}
    <ErrorText error={error} />
    {!primary ? <Text role="status" className="text-sm text-muted">{noPrimary}</Text>
      : !shared.writable ? <Text role="status" className="text-sm text-muted">{shared.writeNotice || '连上后就能发送。'}</Text>
      : <Text className="text-sm text-muted">发给主 Bot「{primary.name}」</Text>}
  </KeyboardAwareScrollView>
}
