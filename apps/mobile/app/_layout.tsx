import '@/platform/polyfills'
import '../global.css'

import { Stack } from 'expo-router'
import { StatusBar } from 'expo-status-bar'
import { GestureHandlerRootView } from 'react-native-gesture-handler'
import { KeyboardProvider } from 'react-native-keyboard-controller'
import { useCSSVariable } from 'uniwind'
import { useWorkspace } from '@/data/context'
import { LiveWorkspace } from '@/data/live'

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
  const ready = workspace.session.phase === 'ready'
  return <>
    <StatusBar style="auto" />
    <Stack screenOptions={{
      headerShadowVisible: false,
      headerBackButtonDisplayMode: 'minimal',
      headerTintColor: foreground,
      headerTitleStyle: { fontFamily: 'Geist', fontWeight: '600', fontSize: 16, color: foreground },
      headerLargeTitleStyle: { fontFamily: 'Geist', fontWeight: '700', color: foreground },
      contentStyle: { backgroundColor: background },
    }}>
      <Stack.Protected guard={!ready}>
        <Stack.Screen name="join" options={{ headerShown: false }} />
      </Stack.Protected>
      <Stack.Protected guard={ready}>
        <Stack.Screen name="index" options={{ title: '消息', headerLargeTitle: true }} />
        <Stack.Screen name="b/[botId]" options={{ title: '' }} />
        <Stack.Screen name="b/[botId]/about" options={{ title: '' }} />
        <Stack.Screen name="b/[botId]/work/[workId]" options={{ title: '任务' }} />
        <Stack.Screen name="new-bot" options={{ title: '新建 bot', presentation: 'modal' }} />
        <Stack.Screen name="ai" options={{ title: 'AI' }} />
        <Stack.Screen name="inbox" options={{ title: '待处理' }} />
        <Stack.Screen name="settings" options={{ title: '设置' }} />
        <Stack.Screen name="device/[deviceId]" options={{ title: '' }} />
        <Stack.Screen name="add-device" options={{ title: '添加设备', presentation: 'modal' }} />
        <Stack.Screen name="recovery" options={{ title: '换恢复密码' }} />
      </Stack.Protected>
    </Stack>
  </>
}
