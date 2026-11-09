import { Stack, router, useLocalSearchParams } from 'expo-router'
import { useEffect } from 'react'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { MessageSquare } from 'lucide-react-native'
import { useSafeAreaFrame, useSafeAreaInsets } from 'react-native-safe-area-context'
import { ConnectionMenu } from '@/components/connection-pill'
import { ScreenFaces } from '@/components/bot-face'
import { EmptyState } from '@/components/kit'
import { useWorkspace } from '@/data/context'
import { HomeList, NewBotButton } from '@/features/home/list'
import { useWide } from '@/lib/layout'

/** The phone's home; on a wide window the list lives in the sidebar and this pane only waits for a choice. */
export default function Home() {
  const wide = useWide()
  const workspace = useWorkspace()
  const { conversation, work, routine } = useLocalSearchParams<{ conversation?: string; work?: string; routine?: string }>()
  const bot = workspace.bots.find((item) => item.conversation_id === conversation)
  useEffect(() => {
    if (!bot || !conversation) return
    if (work) router.replace(`/b/${bot.bot_id}/work/${work}`)
    else router.replace({ pathname: '/b/[botId]', params: { botId: bot.bot_id, ...(routine ? { routine } : {}) } })
  }, [conversation, work, routine, bot?.bot_id])
  const bar = useHeaderHeight()
  const frame = useSafeAreaFrame()
  const insets = useSafeAreaInsets()
  if (wide) return <>
    <Stack.Screen options={{ headerShown: false }} />
    <EmptyState icon={MessageSquare} title="选一个 Bot 开始聊天" className="flex-1 justify-center" />
  </>
  return <>
    <Stack.Screen options={{
      headerShown: true,
      headerLeft: () => <ConnectionMenu connection={workspace.connection} />,
      headerRight: () => <NewBotButton />,
    }} />
    <ScreenFaces><HomeList minHeight={Math.max(0, frame.height - bar - insets.bottom)} /></ScreenFaces>
  </>
}
