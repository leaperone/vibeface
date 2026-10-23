import { Link, Stack, type Href } from 'expo-router'
import { Bot, Inbox, Plus, Settings, Star } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { ConnectionPill } from '@/components/connection-pill'
import { Badge, EmptyState, Monogram } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import { listTime } from '@/lib/time'

/** The phone's home, as on the web: 待处理 first, then every bot by its newest message. */
export default function Home() {
  const workspace = useWorkspace()
  const waiting = workspace.questions.filter((question) => question.answer === null).length
  const bots = [...workspace.bots].sort((a, b) => (b.last_message?.created_at_ms ?? -1) - (a.last_message?.created_at_ms ?? -1))
  const now = Date.now()
  const shared = useLive()
  const canCreate = Boolean(shared && (shared.writable || shared.pendingBot))
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl grow self-center gap-2 px-1 pb-8">
    <Stack.Screen options={{
      headerRight: () => <Link href="/settings" asChild>
        <Button variant="ghost" size="icon" accessibilityLabel="设置"><Icon as={Settings} /></Button>
      </Link>,
    }} />
    <Row href="/inbox" avatar={<View className="size-11 items-center justify-center rounded-full bg-muted"><Icon as={Inbox} className="text-muted-foreground" /></View>} name="待处理" unread={waiting} preview={waiting ? `${waiting} 件事等你决定` : '没有要你决定的事'} />
    <View className="flex-row items-center justify-between ps-3 pt-2">
      <Text role="heading" className="text-meta font-medium text-muted-foreground">bot</Text>
      <Link href="/new-bot" asChild>
        <Button variant="ghost" size="icon" className="size-11" accessibilityLabel="新建 bot" disabled={!canCreate}><Icon as={Plus} /></Button>
      </Link>
    </View>
    {bots.length ? bots.map((bot) => {
      const last = bot.last_message
      return <Row key={bot.bot_id} href={`/b/${bot.bot_id}`} avatar={<Monogram name={bot.name} seed={bot.bot_id} />} name={bot.name}
        mark={workspace.primary === bot.conversation_id ? <Icon as={Star} accessibilityLabel="主 bot" className="size-3.5 fill-muted-foreground text-muted-foreground" /> : null}
        unread={workspace.unread[bot.conversation_id] ?? 0} time={last ? listTime(last.created_at_ms, now) : undefined}
        preview={last ? `${last.role === 'user' ? '你：' : ''}${last.text}` : '还没有消息'} />
    }) : <EmptyState icon={Bot} title="还没有 bot" action={canCreate ? <Link href="/new-bot" asChild>
      <Button className="min-h-11"><Icon as={Plus} /><Text>新建 bot</Text></Button>
    </Link> : undefined} />}
    <View className="mt-auto border-t border-border px-1 pt-2"><ConnectionPill connection={workspace.connection} /></View>
  </ScrollView>
}

function Row({ href, avatar, name, mark, preview, unread, time }: { href: Href; avatar: ReactNode; name: string; mark?: ReactNode; preview: string; unread: number; time?: string }) {
  return <Link href={href} asChild>
    <Pressable className="min-h-16 flex-row items-center gap-3 rounded-xl px-3 py-2.5 active:bg-muted/60">
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
