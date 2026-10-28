import { Link, usePathname, type Href } from 'expo-router'
import { Bot, Inbox, Plus, Settings, Star } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { ConnectionMenu } from '@/components/connection-pill'
import { Badge, EmptyState, Monogram } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { recoveryCopy } from '@vibeface/client/identity/copy.ts'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import { listTime } from '@/lib/time'
import { cn } from '@/lib/utils'

/**
 * The home list, as on the web: 待处理 first, then every bot by its newest message. As a sidebar it draws its own
 * title and settings button in place of the native header, and keeps the open screen's row lit.
 */
export function HomeList({ sidebar }: { sidebar?: boolean }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const pathname = usePathname()
  const insets = useSafeAreaInsets()
  const shared = useLive()
  // Until the first read lands an empty list is not yet known, so rows hold their place instead of 还没有 Bot.
  const loading = !shared?.synced
  const waiting = workspace.questions.filter((question) => question.answer === null).length + Number(!!device.pendingRecovery) + Number(device.rotateHint)
  const bots = [...workspace.bots].sort((a, b) => (b.last_message?.created_at_ms ?? -1) - (a.last_message?.created_at_ms ?? -1))
  const now = Date.now()
  const canCreate = Boolean(shared && (shared.writable || shared.pendingBot))
  const active = (href: string) => sidebar === true && (pathname === href || pathname.startsWith(`${href}/`))
  // The sidebar swaps the open screen rather than stacking another, keeping home underneath for when the window narrows.
  const replace = sidebar === true && pathname !== '/'
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName={cn('gap-2 px-1', sidebar ? 'px-2' : 'w-full max-w-2xl self-center pb-8')}
    contentContainerStyle={sidebar ? { paddingTop: insets.top, paddingBottom: insets.bottom + 32 } : undefined}>
    {sidebar ? <View className="flex-row items-center justify-between ps-3 pt-2">
      <Text role="heading" className="text-3xl font-bold">消息</Text>
      <View className="-me-1 flex-row items-center">
        <ConnectionMenu connection={workspace.connection} />
        <Link href="/settings" replace={replace} asChild><Button variant="ghost" size="icon" className={cn('size-11 rounded-full', active('/settings') && 'bg-muted')} accessibilityLabel="设置" accessibilityState={{ selected: active('/settings') }}><Icon as={Settings} /></Button></Link>
      </View>
    </View> : null}
    <Row href="/inbox" replace={replace} active={active('/inbox')} avatar={<View className="size-11 items-center justify-center rounded-full bg-muted"><Icon as={Inbox} className="text-muted-foreground" /></View>} name="待处理" unread={waiting} preview={device.pendingRecovery ? recoveryCopy.pending : waiting ? `${waiting} 件事等你决定` : loading ? '' : '没有要你决定的事'} />
    <View className="-mb-2 flex-row items-center justify-between ps-3 pt-4">
      <Text role="heading" className="text-meta font-medium text-muted-foreground">Bot</Text>
      <Link href="/new-bot" asChild>
        <Button variant="ghost" size="icon" className="-me-1 size-11" accessibilityLabel="新 Bot" disabled={!canCreate}><Icon as={Plus} /></Button>
      </Link>
    </View>
    {bots.length ? bots.map((bot) => {
      const last = bot.last_message
      return <Row key={bot.bot_id} href={`/b/${bot.bot_id}`} replace={replace} active={active(`/b/${bot.bot_id}`)} avatar={<Monogram name={bot.name} seed={bot.bot_id} />} name={bot.name}
        mark={workspace.primary === bot.conversation_id ? <Icon as={Star} accessibilityLabel="主 Bot" className="size-3.5 fill-muted-foreground text-muted-foreground" /> : null}
        unread={workspace.unread[bot.conversation_id] ?? 0} time={last ? listTime(last.created_at_ms, now) : undefined}
        preview={last ? `${last.role === 'user' ? '你：' : ''}${last.text}` : '还没有消息'} />
    }) : loading ? <View accessibilityLabel="正在读取 Bot" className="gap-2">{[0, 1, 2].map((item) => <View key={item} className="h-16 rounded-xl bg-muted/60" />)}</View> : <EmptyState icon={Bot} title="还没有 Bot" action={canCreate ? <Link href="/new-bot" asChild>
      <Button className="min-h-11"><Icon as={Plus} /><Text>新 Bot</Text></Button>
    </Link> : undefined} />}
  </ScrollView>
}

function Row({ href, replace, active, avatar, name, mark, preview, unread, time }: { href: Href; replace: boolean; active: boolean; avatar: ReactNode; name: string; mark?: ReactNode; preview: string; unread: number; time?: string }) {
  return <Link href={href} replace={replace} asChild>
    <Pressable accessibilityState={{ selected: active }} className={cn('min-h-16 flex-row items-center gap-3 rounded-xl px-3 py-2.5 active:bg-muted', active && 'bg-muted')}>
      {avatar}
      <View className="min-w-0 flex-1 gap-0.5">
        <View className="flex-row items-center gap-2">
          <View className="min-w-0 flex-1 flex-row items-center gap-1.5">
            <Text numberOfLines={1} className="shrink font-medium">{name}</Text>{mark}
          </View>
          {time ? <Text maxFontSizeMultiplier={1.4} className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{time}</Text> : null}
        </View>
        <View className="flex-row items-center gap-2">
          <Text numberOfLines={1} className="min-w-0 flex-1 text-meta text-muted-foreground">{preview}</Text>
          <Badge count={unread} />
        </View>
      </View>
    </Pressable>
  </Link>
}
