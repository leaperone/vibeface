import { Link } from 'expo-router'
import { Bot, ChevronRight, Inbox as InboxIcon } from 'lucide-react-native'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { EmptyState } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { QuestionCard } from '@/features/chat/question-card'

/** Questions nobody answered, newest first; each names its bot and leads to its chat. */
export default function Inbox() {
  const workspace = useWorkspace()
  const open = workspace.questions.filter((question) => question.answer === null).sort((a, b) => b.created_at_ms - a.created_at_ms)
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentContainerClassName="gap-4 p-4">
    {open.length ? open.map((question) => <View key={question.question_id} className="gap-2">
      {question.assistant ? <Link href={`/b/${question.assistant.bot_id}`} asChild>
        <Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-1 self-start">
          <Icon as={Bot} className="size-4" /><Text className="font-medium">{question.assistant.name}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" />
        </Pressable>
      </Link> : null}
      <QuestionCard question={question} />
    </View>) : <EmptyState icon={InboxIcon} title="没有要你决定的事" />}
  </KeyboardAwareScrollView>
}
