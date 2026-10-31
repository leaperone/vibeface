import '@/platform/polyfills'
import '../global.css'

import { useLinkingURL } from 'expo-linking'
import { Stack, router } from 'expo-router'
import { X } from 'lucide-react-native'
import * as SystemUI from 'expo-system-ui'
import { StatusBar } from 'expo-status-bar'
import { useEffect } from 'react'
import { Platform, View } from 'react-native'
import { GestureHandlerRootView } from 'react-native-gesture-handler'
import { KeyboardProvider } from 'react-native-keyboard-controller'
import { useCSSVariable } from 'uniwind'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { useWorkspace } from '@/data/context'
import { LiveWorkspace, useLive } from '@/data/live'
import { HomeList } from '@/features/home/list'
import { openedLink } from '@/lib/join-link'
import { useWide } from '@/lib/layout'

// The join address last answered here, so a remount or a return to the app does not ask again.
let answered: string | null = null

export default function RootLayout() {
  return <GestureHandlerRootView style={{ flex: 1 }}>
    <KeyboardProvider>
      <LiveWorkspace>
        <Screens />
      </LiveWorkspace>
    </KeyboardProvider>
  </GestureHandlerRootView>
}

function Screens() {
  const workspace = useWorkspace()
  const shared = useLive()
  const [background, foreground] = useCSSVariable(['--background', '--foreground']) as [string, string]
  // Mid-rotation, UIKit can show the window behind a view that has not resized yet; paint it the page colour, not white.
  useEffect(() => { void SystemUI.setBackgroundColorAsync(background) }, [background])
  const ready = workspace.session.phase === 'ready'
  const device = useDevice()
  const url = useLinkingURL()
  // The join page is closed to a phone in a network, even one it cannot reach; a link of another network asks to switch.
  useEffect(() => {
    const link = openedLink(url)
    if (!ready || !link || url === answered) return
    answered = url
    if (device.linkTarget(link) === 'other') router.push('/switch-network')
    else router.navigate('/')
  }, [url, ready])
  const wide = useWide()
  // Beside the sidebar, its own screens are roots: no back button or swipe to the empty placeholder.
  const root = wide ? { headerBackVisible: false, gestureEnabled: false } : {}
  // The Stack keeps its place in the tree whether or not the sidebar shows, so a rotation never resets the route.
  return <>
    <StatusBar style="auto" />
    <View className="flex-1 flex-row bg-background">
      {/* Kept mounted and hidden when narrow, so a rotation only flips its display. */}
      {ready ? <View className="w-80 border-e border-border bg-background" style={{ display: wide ? 'flex' : 'none' }}><HomeList sidebar /></View> : null}
      <View className="flex-1">
        <Stack screenListeners={({ route }) => ({ focus: () => shared?.setView(route.name === 'b/[botId]' ? 'chat' : route.name === 'tasks' ? 'tasks' : 'other') })} screenOptions={{
          headerShadowVisible: false,
          // iOS draws its glass bar with a soft scroll edge; Android's bar is a plain surface in the page colour.
          ...Platform.OS === 'android' ? { headerStyle: { backgroundColor: background } } : {},
          headerBackButtonDisplayMode: 'minimal',
          headerTintColor: foreground,
          headerTitleStyle: { fontFamily: 'Geist', fontWeight: '600', fontSize: Platform.OS === 'ios' ? 17 : 20, color: foreground },
          // The native large title takes no letter spacing, so it stays at the font's own.
          headerLargeTitleStyle: { fontFamily: 'Geist', fontWeight: '600', color: foreground },
          contentStyle: { backgroundColor: background },
        }}>
          <Stack.Protected guard={!ready}>
            <Stack.Screen name="join" options={{ headerShown: false }} />
          </Stack.Protected>
          <Stack.Protected guard={ready}>
            <Stack.Screen name="index" options={{ title: '消息', headerLargeTitle: true }} />
            <Stack.Screen name="b/[botId]" options={{ title: '', ...root }} />
            <Stack.Screen name="b/[botId]/about" options={{ title: '' }} />
            <Stack.Screen name="b/[botId]/context" options={{ title: '上下文和协作' }} />
            <Stack.Screen name="b/[botId]/work/[workId]" options={{ title: '任务' }} />
            <Stack.Screen name="tasks" options={{ title: '任务', ...root }} />
            <Stack.Screen name="new-bot" options={{ title: '新 Bot', presentation: 'modal', headerLeft: Close }} />
            {/* Opened from the new bot sheet it is a sheet too, with no back button of its own. */}
            <Stack.Screen name="ai" options={({ route }) => ({ title: 'Bot 与任务', headerLeft: (route.params as { sheet?: string } | undefined)?.sheet ? Close : undefined })} />
            <Stack.Screen name="inbox" options={{ title: '待处理', ...root }} />
            <Stack.Screen name="settings" options={{ title: '设置', ...root }} />
            <Stack.Screen name="lab" options={{ title: '实验室' }} />
            <Stack.Screen name="logs" options={{ title: '日志' }} />
            <Stack.Screen name="external-chat" options={{ title: '外部聊天' }} />
            <Stack.Screen name="transfers" options={{ title: '文件传输' }} />
            <Stack.Screen name="device/[deviceId]" options={{ title: '' }} />
            <Stack.Screen name="add-device" options={{ title: '添加设备', presentation: 'modal', headerLeft: Close }} />
            <Stack.Screen name="recovery" options={{ title: '换恢复密码' }} />
            <Stack.Screen name="switch-network" options={{ headerShown: false, presentation: 'formSheet', sheetAllowedDetents: 'fitToContents', sheetGrabberVisible: true }} />
          </Stack.Protected>
        </Stack>
      </View>
    </View>
  </>
}

/** A sheet's own way out, beside the swipe down that not everyone knows. */
function Close() {
  return <Button variant="ghost" size="icon" className="rounded-full" accessibilityLabel="关闭" onPress={() => router.back()}><Icon as={X} className="size-5" /></Button>
}
