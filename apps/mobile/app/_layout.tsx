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
      headerStyle: { backgroundColor: background },
      headerTintColor: foreground,
      headerTitleStyle: { fontFamily: 'Geist', fontWeight: '600', fontSize: 16 },
      contentStyle: { backgroundColor: background },
    }}>
      <Stack.Protected guard={!ready}>
        <Stack.Screen name="join" options={{ headerShown: false }} />
      </Stack.Protected>
      <Stack.Protected guard={ready}>
        <Stack.Screen name="index" options={{ title: '消息', headerLargeTitle: true }} />
        <Stack.Screen name="b/[botId]" options={{ title: '' }} />
        <Stack.Screen name="inbox" options={{ title: '待处理' }} />
        <Stack.Screen name="settings" options={{ title: '设置' }} />
      </Stack.Protected>
    </Stack>
  </>
}
