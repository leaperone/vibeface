import { useGlobalSearchParams, usePathname } from 'expo-router'
import { X } from 'lucide-react-native'
import { useEffect, useMemo, useState } from 'react'
import { AccessibilityInfo, AppState, Platform, Pressable, View } from 'react-native'
import { FullWindowOverlay } from 'react-native-screens'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { notificationPreferences, notificationPreview, watchNotificationPreferences } from '@vibeface/client/workspace/system-notifications.ts'
import type { NotificationRecord } from '@vibeface/client/workspace/types.ts'
import { Button } from '@/components/arc/button'
import { ToastNotice } from '@/components/arc/feedback'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { useLive, useOpenNotificationConversation } from '@/data/live'
import { mobilePlatform } from '@/platform'
import { foregroundInbox, notificationOnScreen } from '@/platform/foreground-notifications'

export function NotificationBanner({ networkId }: { networkId: string }) {
  const shared = useLive()
  const open = useOpenNotificationConversation()
  const path = usePathname()
  const { routine } = useGlobalSearchParams<{ routine?: string }>()
  const insets = useSafeAreaInsets()
  const [observe] = useState(foregroundInbox)
  const [resume, resumed] = useState(0)
  const [item, setItem] = useState<NotificationRecord | null>(null)
  const [preferenceRevision, refreshPreferences] = useState(0)
  useEffect(() => watchNotificationPreferences(networkId, () => refreshPreferences(n => n + 1)), [networkId])
  useEffect(() => {
    const subscription = AppState.addEventListener('change', state => {
      if (state === 'active') resumed(n => n + 1)
      else setItem(null)
    })
    return () => subscription.remove()
  }, [])
  const notifications = shared?.notifications
  const muted = useMemo(() => notificationPreferences(mobilePlatform.storage, networkId).read().mutedConversations, [networkId, preferenceRevision])
  const bot = (notice: NotificationRecord) => shared?.bots.find(bot => bot.conversation_id === notice.conversation_id)
  const eligible = (notice: NotificationRecord) => !shared?.ignoredNotifications.has(notice.notification_id) && !muted.includes(notice.conversation_id)
    && !notificationOnScreen(notice, path, routine, bot(notice)?.bot_id)
  useEffect(() => {
    if (!shared?.synced || !shared.isCurrent() || !notifications) return
    const next = observe(AppState.currentState, notifications, eligible)
    if (next) setItem(next)
    else setItem(current => current && notifications.some(n => n.notification_id === current.notification_id) && eligible(current) ? current : null)
  }, [notifications, shared?.synced, shared?.ignoredNotifications, shared?.bots, path, routine, muted, resume])
  const title = item ? bot(item)?.name || 'Bot' : ''
  const preview = item ? item.text.trim().slice(0, 180) || notificationPreview(item) : ''
  useEffect(() => {
    if (!item) return
    AccessibilityInfo.announceForAccessibility(title + '，' + preview)
    const timeout = setTimeout(() => setItem(null), 6000)
    return () => clearTimeout(timeout)
  }, [item, title, preview])
  if (!item || !shared?.isCurrent() || !eligible(item)) return null
  const banner = <View pointerEvents="box-none" className="absolute items-center px-3" style={{ top: insets.top + 4, left: insets.left, right: insets.right }}>
    <ToastNotice>
      <Pressable accessibilityRole="button" accessibilityLabel={title + '，' + preview} accessibilityHint="打开对话" className="min-h-12 min-w-0 flex-1 justify-center" onPress={() => {
        setItem(null)
        if (AppState.currentState === 'active' && shared.isCurrent()) open(item.conversation_id, item.target, item.focus, item.notification_id)
      }}>
        <Text numberOfLines={1} className="font-medium text-foreground">{title}</Text>
        <Text numberOfLines={2} className="text-sm text-muted">{preview}</Text>
      </Pressable>
      <Button variant="ghost" iconOnly accessibilityLabel="关闭提醒" onPress={() => setItem(null)}><Icon as={X} className="size-4 text-muted" /></Button>
    </ToastNotice>
  </View>
  return Platform.OS === 'ios' ? <FullWindowOverlay unstable_accessibilityContainerViewIsModal={false}>{banner}</FullWindowOverlay> : banner
}
