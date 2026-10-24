import { Link } from 'expo-router'
import { Bot, ChevronRight, Inbox as InboxIcon } from 'lucide-react-native'
import { Pressable } from 'react-native'
import Animated, { FadeOut, LinearTransition, ReduceMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { EmptyState } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { QuestionCard } from '@/features/chat/question-card'

const Group = withUniwind(Animated.View)
// Answered cards fade out and the rest glide into place, so a group moving to the top reads as a move, not a jump.
const move = LinearTransition.springify().dampingRatio(1).duration(300).reduceMotion(ReduceMotion.System)
const leave = FadeOut.duration(150).reduceMotion(ReduceMotion.System)

/** Questions nobody answered, grouped by bot with the newest group first; each group names its bot once and leads to its chat. */
export default function Inbox() {
  const workspace = useWorkspace()
  const open = workspace.questions.filter((question) => question.answer === null).sort((a, b) => b.created_at_ms - a.created_at_ms)
  const groups = new Map<string, typeof open>()
  for (const question of open) groups.set(question.bot?.bot_id ?? '', [...groups.get(question.bot?.bot_id ?? '') ?? [], question])
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-4 p-4 pb-10">
    {open.length ? [...groups.values()].map((questions) => <Group key={questions[0].bot?.bot_id ?? ''} layout={move} exiting={leave} className="gap-2">
      {questions[0].bot ? <Link href={`/b/${questions[0].bot.bot_id}`} asChild>
        <Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-1 self-start">
          <Icon as={Bot} className="size-4" /><Text className="font-medium">{questions[0].bot.name}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" />
        </Pressable>
      </Link> : null}
      {questions.map((question) => <Animated.View key={question.question_id} layout={move} exiting={leave}><QuestionCard question={question} /></Animated.View>)}
    </Group>) : <EmptyState icon={InboxIcon} title="没有要你决定的事" />}
  </KeyboardAwareScrollView>
}
