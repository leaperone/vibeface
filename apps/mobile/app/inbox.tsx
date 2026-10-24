import { Link } from 'expo-router'
import { useHeaderHeight } from 'expo-router/react-navigation'
import { Bot, ChevronRight, Inbox as InboxIcon } from 'lucide-react-native'
import { useRef, useState } from 'react'
import { Platform, Pressable, View } from 'react-native'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import Animated, { LinearTransition, ReduceMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { EmptyState } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useWide } from '@/lib/layout'
import { QuestionCard } from '@/features/chat/question-card'

const Group = withUniwind(Animated.View)
// An answered card leaves at once and what was below glides up into its space, so nothing passes over anything.
const move = LinearTransition.springify().dampingRatio(1).duration(300).reduceMotion(ReduceMotion.System)

/** Questions nobody answered, grouped by bot; each group names its bot once and leads to its chat. */
export default function Inbox() {
  const workspace = useWorkspace()
  const wide = useWide()
  const bar = useHeaderHeight()
  const insets = useSafeAreaInsets()
  const [frame, setFrame] = useState(0)
  // Groups order by the newest question each bot has shown here. Answered questions leave the list, so the time is kept
  // and only rises: answering never reshuffles the groups, and only a new question lifts its bot to the top.
  const latest = useRef(new Map<string, number>()).current
  const groups = new Map<string, typeof workspace.questions>()
  for (const question of workspace.questions.filter((item) => item.answer === null).sort((a, b) => b.created_at_ms - a.created_at_ms)) {
    const key = question.bot?.bot_id ?? ''
    latest.set(key, Math.max(latest.get(key) ?? 0, question.created_at_ms))
    groups.set(key, [...groups.get(key) ?? [], question])
  }
  const open = [...groups].sort(([a], [b]) => latest.get(b)! - latest.get(a)!).map(([, questions]) => questions)
  // The scroll view stays when empty: iOS tints the glass back button from it, and swapping it out flashes the button light.
  // The empty state fills what shows below the bar, which on iOS the automatic inset leaves out of the view's own height.
  // Its extra 24 on top evens out the container's 16 top and 40 bottom padding, so it centres in that area.
  const below = Math.max(0, frame - (Platform.OS === 'ios' ? bar + insets.bottom : 0) - 56)
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-4 p-4 pb-10"
    onLayout={(event) => setFrame(event.nativeEvent.layout.height)}>
    {open.length ? open.map((questions) => <Group key={questions[0].bot?.bot_id ?? ''} layout={move} className="gap-2">
      {questions[0].bot ? <Link href={`/b/${questions[0].bot.bot_id}`} replace={wide} asChild>
        <Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-1 self-start">
          <Icon as={Bot} className="size-4" /><Text className="font-medium">{questions[0].bot.name}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" />
        </Pressable>
      </Link> : null}
      {questions.map((question) => <Animated.View key={question.question_id} layout={move}><QuestionCard question={question} /></Animated.View>)}
    </Group>) : <View style={{ minHeight: below, paddingTop: 24 }} className="justify-center"><EmptyState icon={InboxIcon} title="没有要你决定的事" /></View>}
  </KeyboardAwareScrollView>
}
