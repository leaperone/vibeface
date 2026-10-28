import { Link, Stack } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { MessageSquare, Plus, Settings } from 'lucide-react-native'
import { View } from 'react-native'
import { useSafeAreaFrame, useSafeAreaInsets } from 'react-native-safe-area-context'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { useLive } from '@/data/live'
import { HomeList } from '@/features/home/list'
import { useWide } from '@/lib/layout'

/** The phone's home; on a wide window the list lives in the sidebar and this pane only waits for a choice. */
export default function Home() {
  const wide = useWide()
  const bar = useHeaderHeight()
  const frame = useSafeAreaFrame()
  const insets = useSafeAreaInsets()
  const shared = useLive()
  const canCreate = Boolean(shared && (shared.writable || shared.pendingBot))
  if (wide) return <>
    <Stack.Screen options={{ headerShown: false }} />
    <View className="flex-1 justify-center"><EmptyState icon={MessageSquare} title="选一个 Bot 开始聊天" /></View>
  </>
  return <>
    <Stack.Screen options={{
      headerShown: true,
      headerRight: () => <View className="flex-row">
        <Link href="/new-bot" asChild><Button variant="ghost" size="icon" accessibilityLabel="新 Bot" disabled={!canCreate}><Icon as={Plus} /></Button></Link>
        <Link href="/settings" asChild><Button variant="ghost" size="icon" accessibilityLabel="设置"><Icon as={Settings} /></Button></Link>
      </View>,
    }} />
    {/* The list fills only what shows below the bar, so the connection footer rests at the bottom rather than past it. */}
    <HomeList minHeight={Math.max(0, frame.height - bar - insets.bottom)} />
  </>
}
