import '@/platform/polyfills'
import '../global.css'

import { Stack } from 'expo-router'
import * as SystemUI from 'expo-system-ui'
import { StatusBar } from 'expo-status-bar'
import { useEffect } from 'react'
import { Platform, View } from 'react-native'
import { GestureHandlerRootView } from 'react-native-gesture-handler'
import { KeyboardProvider } from 'react-native-keyboard-controller'
import { useCSSVariable } from 'uniwind'
import { useWorkspace } from '@/data/context'
import { LiveWorkspace } from '@/data/live'
import { HomeList } from '@/features/home/list'
import { useWide } from '@/lib/layout'

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
  const [background, foreground] = useCSSVariable(['--background', '--foreground']) as [string, string]
  // Mid-rotation, UIKit can show the window behind a view that has not resized yet; paint it the page colour, not white.
  useEffect(() => { void SystemUI.setBackgroundColorAsync(background) }, [background])
  const ready = workspace.session.phase === 'ready'
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
        <Stack screenOptions={{
          headerShadowVisible: false,
          // iOS draws its glass bar with a soft scroll edge; Android's bar is a plain surface in the page colour.
          ...Platform.OS === 'android' ? { headerStyle: { backgroundColor: background } } : {},
          headerBackButtonDisplayMode: 'minimal',
          headerTintColor: foreground,
          headerTitleStyle: { fontFamily: 'Geist', fontWeight: '600', fontSize: Platform.OS === 'ios' ? 17 : 20, color: foreground },
          headerLargeTitleStyle: { fontFamily: 'Geist', fontWeight: '700', color: foreground },
          contentStyle: { backgroundColor: background },
        }}>
          <Stack.Protected guard={!ready}>
            <Stack.Screen name="join" options={{ headerShown: false }} />
          </Stack.Protected>
          <Stack.Protected guard={ready}>
            <Stack.Screen name="index" options={{ title: '消息', headerLargeTitle: true }} />
            <Stack.Screen name="b/[botId]" options={{ title: '', ...root }} />
            <Stack.Screen name="b/[botId]/about" options={{ title: '' }} />
            <Stack.Screen name="b/[botId]/work/[workId]" options={{ title: '任务' }} />
            <Stack.Screen name="new-bot" options={{ title: '新 Bot', presentation: 'modal' }} />
            <Stack.Screen name="ai" options={{ title: 'Bot 与任务' }} />
            <Stack.Screen name="inbox" options={{ title: '待处理', ...root }} />
            <Stack.Screen name="settings" options={{ title: '设置', ...root }} />
            <Stack.Screen name="transfers" options={{ title: '文件传输' }} />
            <Stack.Screen name="device/[deviceId]" options={{ title: '' }} />
            <Stack.Screen name="add-device" options={{ title: '添加设备', presentation: 'modal' }} />
            <Stack.Screen name="recovery" options={{ title: '换恢复密码' }} />
          </Stack.Protected>
        </Stack>
      </View>
    </View>
  </>
}
