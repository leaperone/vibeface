import { Link, type Href } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { Bot, ChevronRight, CircleAlert, CircleX, Copy, LoaderCircle, X, Inbox as InboxIcon, Lock, MessageSquareWarning, Sparkles, type LucideIcon } from 'lucide-react-native'
import { useRef, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { useSafeAreaFrame, useSafeAreaInsets } from 'react-native-safe-area-context'
import Animated, { LayoutAnimationConfig } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { ErrorText, useAction } from '@/components/action'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import type { InboxItem } from '@vibeface/client/workspace/inbox.ts'
import type { Question } from '@vibeface/client/workspace/types.ts'
import { useInbox, useLive } from '@/data/live'
import { useWide } from '@/lib/layout'
import { move } from '@/lib/motion'
import { QuestionCard } from '@/features/chat/question-card'
import { ConfirmCard } from '@/features/chat/work'
import { JoinRequests } from '@/features/devices/join-requests'
import { RecoveryPendingCard, RotateHintCard } from '@/features/recovery/recovery'

// An answered card leaves at once and what was below glides up into its space, so nothing passes over anything.
const Group = withUniwind(Animated.View)

type Row = Extract<InboxItem, { kind: 'write' | 'undelivered' | 'attention' | 'setup' | 'failed' }>
const rowIcons: Record<Row['kind'], LucideIcon> = { write: Copy, undelivered: CircleAlert, attention: MessageSquareWarning, setup: Sparkles, failed: CircleX }

/**
 * The items the web lists: a recovery to stop first, cards to act on here, rows leading to where the rest is settled, join
 * requests, then questions nobody answered, grouped by bot; each group names its bot once and leads to its chat.
 */
export default function Inbox() {
  const items = useInbox()
  const device = useDevice()
  const shared = useLive()
  const wide = useWide()
  const bar = useHeaderHeight()
  const insets = useSafeAreaInsets()
  const frame = useSafeAreaFrame()
  // Groups order by the newest question each bot has shown here. Answered questions leave the list, so the time is kept
  // and only rises: answering never reshuffles the groups, and only a new question lifts its bot to the top.
  const latest = useRef(new Map<string, number>()).current
  const groups = new Map<string, Question[]>()
  for (const question of items.flatMap((item) => item.kind === 'question' ? [item.question] : []).sort((a, b) => b.created_at_ms - a.created_at_ms)) {
    const key = question.bot?.bot_id ?? ''
    latest.set(key, Math.max(latest.get(key) ?? 0, question.created_at_ms))
    groups.set(key, [...groups.get(key) ?? [], question])
  }
  const open = [...groups].sort(([a], [b]) => latest.get(b)! - latest.get(a)!).map(([, questions]) => questions)
  // The scroll view stays when empty: iOS tints the glass back button from it, and swapping it out flashes the button light.
  // The empty state fills what shows below the bar, known from the first frame so it never starts at the top.
  // Its extra 24 on top evens out the container's 16 top and 40 bottom padding, so it centres in that area.
  const below = Math.max(0, frame.height - bar - insets.bottom - 56)
  // Entering animations skip the first render: an inbox opened empty just shows it, one emptied by an answer fades it in.
  return <LayoutAnimationConfig skipEntering><KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-3 p-4 pb-10">
    {items.map((item) => item.kind === 'recovery' ? <RecoveryPendingCard key={item.key} />
      : item.kind === 'rotate' ? <RotateHintCard key={item.key} />
      : item.kind === 'confirm' ? <ConfirmCard key={item.key} check={item.check} />
      : item.kind === 'write' || item.kind === 'undelivered' || item.kind === 'attention' || item.kind === 'setup' || item.kind === 'failed' ? <ItemRow key={item.key} item={item} href={rowHref(item, shared?.selectedBot?.bot_id, device.ready?.entry.device_id)} replace={wide} />
      : null)}
    {items.some((item) => item.kind === 'join') ? <JoinRequests /> : null}
    {open.length ? open.map((questions) => <Group key={questions[0].bot?.bot_id ?? ''} layout={move} className="gap-2">
      {questions[0].bot ? <Link href={`/b/${questions[0].bot.bot_id}`} replace={wide} asChild>
        <Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-1 self-start">
          <Icon as={Bot} className="size-4" /><Text className="font-medium">{questions[0].bot.name}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" />
        </Pressable>
      </Link> : null}
      {questions.map((question) => <Animated.View key={question.question_id} layout={move}><QuestionCard question={question} /></Animated.View>)}
    </Group>) : items.length ? null : <View style={{ minHeight: below, paddingTop: 24, justifyContent: 'center' }}>
      <EmptyState icon={InboxIcon} title="没有要你决定的事" />
    </View>}
  </KeyboardAwareScrollView></LayoutAnimationConfig>
}

/** Where a row leads on the phone; the phone cannot settle a copied or read-only computer, so that row only says it. */
function rowHref(item: Row, chat: string | undefined, entry: string | undefined): Href | null {
  if (item.kind === 'attention') return `/b/${item.bot.bot_id}`
  if (item.kind === 'failed') return `/b/${item.bot.bot_id}/work/${item.work.work_id}`
  if (item.kind === 'undelivered') return chat ? `/b/${chat}` : null
  if (item.kind === 'setup') return !item.device || item.device === entry ? '/ai' : `/device/${item.device}`
  return null
}

function ItemRow({ item, href, replace }: { item: Row; href: Href | null; replace: boolean }) {
  const card = 'min-h-14 flex-row items-center gap-3 rounded-xl border border-border bg-card px-4 py-3 shadow-xs dark:border-line-strong'
  const body = <>
    <Icon as={item.kind === 'write' && item.write === 'read_only' ? Lock : rowIcons[item.kind]} className={item.kind === 'failed' ? 'size-5 text-danger' : 'size-5 text-warning'} />
    <Text numberOfLines={2} className="min-w-0 flex-1 font-medium">{item.title}</Text>
    {item.meta ? <Text numberOfLines={1} className="max-w-[40%] text-meta text-muted-foreground">{item.meta}</Text> : null}
    {href ? <Icon as={ChevronRight} className="text-muted-foreground" /> : null}
  </>
  const row = href ? <Link href={href} replace={replace} asChild><Pressable accessibilityRole="link" className={`${card} active:bg-muted`}>{body}</Pressable></Link> : <View className={card}>{body}</View>
  return item.kind === 'failed' ? <Dismiss item={item}>{row}</Dismiss> : row
}

/** A failure the person has seen to leaves the list by hand; the bot's later reply also clears it. */
function Dismiss({ item, children }: { item: Extract<Row, { kind: 'failed' }>; children: ReactNode }) {
  const shared = useLive()
  const { busy, error, run } = useAction()
  return <View className="gap-1">
    <View className="flex-row items-center gap-1">
      <View className="min-w-0 flex-1">{children}</View>
      <Button variant="ghost" size="icon" accessibilityLabel="关掉" disabled={!shared?.writable || busy !== null} onPress={() => void run('dismiss', () => shared!.dismissWork(item.work.conversation_id, item.work.work_id))}>
        <Icon as={busy ? LoaderCircle : X} className="text-muted-foreground" />
      </Button>
    </View>
    <ErrorText error={error} />
  </View>
}
