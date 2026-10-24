import { Link } from 'expo-router'
import { Bot, ChevronRight, Inbox as InboxIcon } from 'lucide-react-native'
import { Pressable } from 'react-native'
import Animated, { LinearTransition, ReduceMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { EmptyState } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { QuestionCard } from '@/features/chat/question-card'

const Group = withUniwind(Animated.View)
// An answered card leaves at once and what was below glides up into its space, so nothing passes over anything.
const move = LinearTransition.springify().dampingRatio(1).duration(300).reduceMotion(ReduceMotion.System)

/** Questions nobody answered, grouped by bot; each group names its bot once and leads to its chat. */
export default function Inbox() {
  const workspace = useWorkspace()
  // Groups order by each bot's latest question, answered or not, so answering never reshuffles them; only a new question does.
  const groups = new Map<string, typeof workspace.questions>()
  for (const question of [...workspace.questions].sort((a, b) => b.created_at_ms - a.created_at_ms)) {
    const key = question.bot?.bot_id ?? ''
    groups.set(key, [...groups.get(key) ?? [], ...question.answer === null ? [question] : []])
  }
  const open = [...groups.values()].filter((questions) => questions.length)
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-4 p-4 pb-10">
    {open.length ? open.map((questions) => <Group key={questions[0].bot?.bot_id ?? ''} layout={move} className="gap-2">
      {questions[0].bot ? <Link href={`/b/${questions[0].bot.bot_id}`} asChild>
        <Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-1 self-start">
          <Icon as={Bot} className="size-4" /><Text className="font-medium">{questions[0].bot.name}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" />
        </Pressable>
      </Link> : null}
      {questions.map((question) => <Animated.View key={question.question_id} layout={move}><QuestionCard question={question} /></Animated.View>)}
    </Group>) : <EmptyState icon={InboxIcon} title="没有要你决定的事" />}
  </KeyboardAwareScrollView>
}
