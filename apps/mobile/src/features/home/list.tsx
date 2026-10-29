import { Link, usePathname, type Href } from 'expo-router'
import { Bot, ChevronRight, Inbox, Plus, Settings, Star } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import Animated, { FadeIn, FadeOut, LayoutAnimationConfig, LinearTransition, ReduceMotion } from 'react-native-reanimated'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { useUniwind, withUniwind } from 'uniwind'
import { ConnectionMenu } from '@/components/connection-pill'
import { botMood, BotFace } from '@/components/bot-face'
import { faceOf } from '@vibeface/client/avatar/face.ts'
import { Badge, botTint, EmptyState, Skeleton } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { recoveryCopy } from '@vibeface/client/identity/copy.ts'
import { moodLabel } from '@vibeface/client/avatar/mood.ts'
import { useWorkspace } from '@/data/context'
import { useInbox, useLive } from '@/data/live'
import { waitingItems } from '@vibeface/client/workspace/inbox.ts'
import { listTime } from '@/lib/time'
import { cn } from '@/lib/utils'

const Item = withUniwind(Animated.View)
// A bot lifted to the top by a new message, or rows closing over the 待处理 card, glide instead of jumping.
const move = LinearTransition.springify().dampingRatio(1).duration(300).reduceMotion(ReduceMotion.System)
const fadeIn = FadeIn.duration(200).reduceMotion(ReduceMotion.System)
const fadeOut = FadeOut.duration(150).reduceMotion(ReduceMotion.System)

/**
 * The home list: a 待处理 card while something waits, then every bot by its newest message. As a sidebar it draws its
 * own title, new-bot and settings buttons in place of the native header, and keeps the open screen's row lit.
 */
export function HomeList({ sidebar, minHeight }: { sidebar?: boolean; minHeight?: number }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const dark = useUniwind().theme === 'dark'
  const pathname = usePathname()
  const insets = useSafeAreaInsets()
  const shared = useLive()
  // Until the first read lands an empty list is not yet known, so rows hold their place instead of 还没有 Bot.
  const loading = !shared?.synced
  const waiting = waitingItems(useInbox()).length
  const bots = [...workspace.bots].sort((a, b) => (b.last_message?.created_at_ms ?? -1) - (a.last_message?.created_at_ms ?? -1))
  const now = Date.now()
  const canCreate = Boolean(shared && (shared.writable || shared.pendingBot))
  const active = (href: string) => sidebar === true && (pathname === href || pathname.startsWith(`${href}/`))
  // The sidebar swaps the open screen rather than stacking another, keeping home underneath for when the window narrows.
  const replace = sidebar === true && pathname !== '/'
  // Entering animations skip the first render, so only a 待处理 card that appears later fades in.
  return <LayoutAnimationConfig skipEntering><ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName={cn('px-2', sidebar ? 'grow' : 'w-full max-w-3xl self-center pb-8')}
    contentContainerStyle={sidebar ? { paddingTop: insets.top, paddingBottom: insets.bottom + 32 } : { minHeight }}>
    {sidebar ? <View className="mb-2 flex-row items-center ps-3 pt-2">
      <Text role="heading" className="flex-1 text-title font-semibold">消息</Text>
      <ConnectionMenu connection={workspace.connection} />
      <Link href="/new-bot" asChild><Button variant="ghost" size="icon" className="size-11 rounded-full" accessibilityLabel="新 Bot" disabled={!canCreate}><Icon as={Plus} /></Button></Link>
      <Link href="/settings" replace={replace} asChild><Button variant="ghost" size="icon" className={cn('-me-1 size-11 rounded-full', active('/settings') && 'bg-muted')} accessibilityLabel="设置" accessibilityState={{ selected: active('/settings') }}><Icon as={Settings} className="size-5" /></Button></Link>
    </View> : null}
    {waiting ? <Item key="inbox" layout={move} entering={fadeIn} exiting={fadeOut} className="mb-2 mt-1">
      <Link href="/inbox" replace={replace} asChild>
        <Pressable accessibilityState={{ selected: active('/inbox') }} className={cn('min-h-16 flex-row items-center gap-3 rounded-2xl border border-warning-border bg-warning-soft px-3 py-2.5 active:opacity-70', active('/inbox') && 'border-warning')}>
          <View className="size-11 items-center justify-center rounded-full bg-warning/15"><Icon as={Inbox} className="text-warning" /></View>
          <Text numberOfLines={1} className="min-w-0 flex-1 font-semibold">{device.pendingRecovery ? recoveryCopy.pending : `${waiting} 件事等你决定`}</Text>
          <Icon as={ChevronRight} className="text-muted-foreground" />
        </Pressable>
      </Link>
    </Item> : null}
    {bots.length ? bots.map((bot, index) => {
      const last = bot.last_message, mood = botMood(bot, workspace, shared), primary = workspace.primary === bot.conversation_id
      return <Item key={bot.bot_id} layout={move}>
        <Row href={`/b/${bot.bot_id}`} replace={replace} active={active(`/b/${bot.bot_id}`)} avatar={<BotFace botId={bot.bot_id} mood={mood} size={44} />} name={bot.name} tint={botTint(faceOf(bot.bot_id).hue, dark)}
          mark={primary ? <Icon as={Star} className="size-3.5 fill-muted-foreground text-muted-foreground" /> : null}
          label={[bot.name, primary ? '主 Bot' : '', moodLabel[mood]].filter(Boolean).join('，')}
          unread={workspace.unread[bot.conversation_id] ?? 0} time={last ? listTime(last.created_at_ms, now) : undefined}
          preview={last ? `${last.role === 'user' ? '你：' : ''}${last.text}` : '还没有消息'} />
        {index < bots.length - 1 ? <View className="ms-[68px] h-px bg-border" /> : null}
      </Item>
    }) : loading ? <View accessibilityLabel="正在读取 Bot">{[0, 1, 2].map((item) => <View key={item} className="min-h-16 flex-row items-center gap-3 px-3 py-2.5">
      <Skeleton className="size-11 rounded-full" />
      <View className="flex-1 gap-2"><Skeleton className="h-3.5 w-1/3 rounded-full" /><Skeleton className="h-3 w-2/3 rounded-full" /></View>
    </View>)}</View> : <View className="flex-1 justify-center"><EmptyState icon={Bot} title="还没有 Bot" action={canCreate ? <Link href="/new-bot" asChild>
      <Button className="min-h-11"><Icon as={Plus} /><Text>新 Bot</Text></Button>
    </Link> : undefined} /></View>}
  </ScrollView></LayoutAnimationConfig>
}

function Row({ href, replace, active, avatar, name, mark, label, preview, unread, time, tint }: { href: Href; replace: boolean; active: boolean; avatar: ReactNode; name: string; mark?: ReactNode; label?: string; preview: string; unread: number; time?: string; tint: { fill: string; ink: string } }) {
  return <Link href={href} replace={replace} asChild>
    <Pressable accessibilityLabel={label && [label, preview, time, unread ? `${unread} 条未读` : ''].filter(Boolean).join('，')} accessibilityState={{ selected: active }} className={cn('min-h-16 flex-row items-center gap-3 rounded-xl px-3 py-2.5 active:bg-muted', active && 'bg-muted')}>
      {avatar}
      <View className="min-w-0 flex-1 gap-0.5">
        <View className="flex-row items-center gap-2">
          <View className="min-w-0 flex-1 flex-row items-center gap-1.5"><Text numberOfLines={1} className={cn('shrink', unread ? 'font-semibold' : 'font-medium')}>{name}</Text>{mark}</View>
          {time ? <Text maxFontSizeMultiplier={1.4} className="text-meta text-muted-foreground" style={[{ fontVariant: ['tabular-nums'] }, unread ? { color: tint.fill } : null]}>{time}</Text> : null}
        </View>
        <View className="flex-row items-center gap-2">
          <Text numberOfLines={1} className={cn('min-w-0 flex-1 text-meta', unread ? 'text-foreground' : 'text-muted-foreground')}>{preview}</Text>
          <Badge count={unread} fill={tint.fill} ink={tint.ink} />
        </View>
      </View>
    </Pressable>
  </Link>
}
