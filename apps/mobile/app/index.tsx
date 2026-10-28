import { Link, Stack } from 'expo-router'
import { MessageSquare, Settings } from 'lucide-react-native'
import { View } from 'react-native'
import { ConnectionMenu } from '@/components/connection-pill'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { useWorkspace } from '@/data/context'
import { HomeList } from '@/features/home/list'
import { useWide } from '@/lib/layout'

/** The phone's home; on a wide window the list lives in the sidebar and this pane only waits for a choice. */
export default function Home() {
  const wide = useWide()
  const workspace = useWorkspace()
  if (wide) return <>
    <Stack.Screen options={{ headerShown: false }} />
    <View className="flex-1 justify-center"><EmptyState icon={MessageSquare} title="选一个 Bot 开始聊天" /></View>
  </>
  return <>
    <Stack.Screen options={{
      headerShown: true,
      headerLeft: () => <ConnectionMenu connection={workspace.connection} />,
      headerRight: () => <Link href="/settings" asChild>
        <Button variant="ghost" size="icon" accessibilityLabel="设置"><Icon as={Settings} /></Button>
      </Link>,
    }} />
    <HomeList />
  </>
}
