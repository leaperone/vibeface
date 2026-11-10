import '@/platform/polyfills'
import '../global.css'

import { useLinkingURL } from 'expo-linking'
import { Link, Stack, router, usePathname } from 'expo-router'
import { Inbox, ListChecks, MessageSquare, Settings, X } from 'lucide-react-native'
import * as SystemUI from 'expo-system-ui'
import { StatusBar } from 'expo-status-bar'
import { useEffect } from 'react'
import { Platform, Pressable, View } from 'react-native'
import { GestureHandlerRootView } from 'react-native-gesture-handler'
import { KeyboardProvider } from 'react-native-keyboard-controller'
import { useCSSVariable } from 'uniwind'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { factoryReset } from '@/lib/factory-reset'
import { Badge, Toaster } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { LiveWorkspace, useLive, useWaiting } from '@/data/live'
import { HomeList } from '@/features/home/list'
import { openedLink } from '@/lib/join-link'
import { useWide } from '@/lib/layout'

// The join address last answered here, so a remount or a return to the app does not ask again.
let answered: string | null = null

const tabs = [['/', '消息', MessageSquare], ['/tasks', '任务', ListChecks], ['/inbox', '待处理', Inbox], ['/settings', '设置', Settings]] as const
// A tab bar press swaps its screen in at once; only a push from inside a screen slides.
const viaTab = ({ route }: { route: { params?: object } }) => route.params && 'tab' in route.params ? { animation: 'none' as const } : {}

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
    if (device.linkTarget(link) === 'other') router.push({ pathname: '/switch-network', params: { link } })
    else router.navigate('/')
  }, [url, ready])
  const wide = useWide()
  const pathname = usePathname()
  const insets = useSafeAreaInsets()
  const waiting = useWaiting()
  const tabbed = ready && !wide && tabs.some(([href]) => href === pathname)
  // Beside the sidebar, its own screens are roots: no back button or swipe to the empty placeholder.
  const root = wide ? { headerBackVisible: false, gestureEnabled: false } : {}
  if (device.phase === 'erasing' || device.phase === 'erase_failed') return <View className="flex-1 justify-center gap-4 p-6">
    <Text role="heading" className="text-heading font-semibold">{device.phase === 'erasing' ? '正在清除应用内数据' : '应用内数据尚未清除完成'}</Text>
    <Text>{device.error || '等待文件选择、分享和保存结束后继续清除。'}</Text>
    <Button disabled={device.phase === 'erasing'} onPress={() => void factoryReset(device).catch(() => {})}><Text>继续清除</Text></Button>
  </View>
  // The Stack keeps its place in the tree whether or not the sidebar shows, so a rotation never resets the route.
  return <>
    <StatusBar style="auto" />
    <View className="flex-1 flex-row bg-background">
      {/* Kept mounted and hidden when narrow, so a rotation only flips its display. */}
      {ready ? <View className="w-80 border-e border-border bg-background" style={{ display: wide ? 'flex' : 'none' }}><HomeList sidebar /></View> : null}
      <View className="flex-1">
        <Stack screenListeners={({ route }) => ({ focus: () => { if (route.name !== 'b/[botId]') shared?.setView(route.name === 'tasks' ? 'tasks' : 'other') } })} screenOptions={{
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
            <Stack.Screen name="index" options={(props) => ({ title: '消息', headerLargeTitle: true, ...viaTab(props) })} />
            <Stack.Screen name="b/[botId]" options={{ title: '', ...root }} />
            <Stack.Screen name="b/[botId]/about" options={{ title: '' }} />
            <Stack.Screen name="b/[botId]/context" options={{ title: '上下文和协作' }} />
            <Stack.Screen name="b/[botId]/work/[workId]" options={{ title: '任务' }} />
            <Stack.Screen name="tasks" options={(props) => ({ title: '任务', ...root, ...viaTab(props) })} />
            <Stack.Screen name="new-task" options={{ title: '新任务' }} />
            <Stack.Screen name="task/[taskId]" options={{ title: '任务对话' }} />
            <Stack.Screen name="task/[taskId]/context" options={{ title: '任务上下文和历史' }} />
            <Stack.Screen name="channels" options={{ title: '频道' }} />
            <Stack.Screen name="new-channel" options={{ title: '新频道' }} />
            <Stack.Screen name="channel/[channelId]" options={{ title: '频道' }} />
            <Stack.Screen name="channel/[channelId]/settings" options={{ title: '频道设置' }} />
            <Stack.Screen name="new-bot" options={{ title: '新 Bot', presentation: 'modal', headerLeft: Close }} />
            {/* Opened from the new bot sheet it is a sheet too, with no back button of its own. */}
            <Stack.Screen name="ai" options={({ route }) => ({ title: 'Bot 与任务', headerLeft: (route.params as { sheet?: string } | undefined)?.sheet ? Close : undefined })} />
            <Stack.Screen name="inbox" options={(props) => ({ title: '待处理', ...root, ...viaTab(props) })} />
            <Stack.Screen name="settings" options={(props) => ({ title: '设置', ...root, ...viaTab(props) })} />
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
        {tabbed ? <View accessibilityRole="tabbar" className="flex-row border-t border-border bg-background px-3 pt-1" style={{ paddingBottom: insets.bottom }}>
          {tabs.map(([href, label, icon]) => {
            const selected = pathname === href
            const tint = selected ? 'text-foreground' : 'text-muted-foreground'
            // iOS reads buttons inside a tabbar as tabs ("tab, 1 of 4"); RN's role="tab" maps to no trait. `role` because only it
            // overrides the role="link" the Link hands its child.
            return <Link key={href} href={{ pathname: href, params: { tab: '1' } }} replace asChild>
              <Pressable role="button" accessibilityState={{ selected }} accessibilityLabel={href === '/inbox' && waiting ? `${label}，${waiting} 件事等你决定` : label} className="min-h-14 flex-1 items-center justify-center gap-1">
                <View>
                  <Icon as={icon} className={`size-5 ${tint}`} />
                  {href === '/inbox' ? <View className="absolute start-3 -top-1.5"><Badge count={waiting} /></View> : null}
                </View>
                <Text className={`text-meta font-medium ${tint}`}>{label}</Text>
              </Pressable>
            </Link>
          })}
        </View> : null}
        {/* ponytail: a fixed lift clears the tab bar or a one-line composer; measure the composer if a taller one gets covered. */}
        <Toaster bottom={(tabbed ? 68 : 88) + insets.bottom} />
      </View>
    </View>
  </>
}

/** A sheet's own way out, beside the swipe down that not everyone knows. */
function Close() {
  return <Button variant="ghost" size="icon" className="rounded-full" accessibilityLabel="关闭" onPress={() => router.back()}><Icon as={X} className="size-5" /></Button>
}
