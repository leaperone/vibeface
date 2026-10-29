import * as Haptics from 'expo-haptics'
import { humanError } from '@vibeface/client/lib/errors.ts'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { ArrowUp, CircleCheck, FolderOpen, Info, Laptop, LoaderCircle, MessageCircleQuestionMark, PencilLine, TriangleAlert, type LucideIcon } from 'lucide-react-native'
import { useState } from 'react'
import { Pressable, TextInput, View } from 'react-native'
import { ErrorText } from '@/components/action'
import { ActionCard, providerIcons, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { caret } from '@/components/ui/input'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Question } from '@/data/types'
import { cn } from '@/lib/utils'

const looks: Record<Question['severity'], { icon: LucideIcon; tone: Tone }> = {
  urgent: { icon: TriangleAlert, tone: 'warning' },
  attention: { icon: MessageCircleQuestionMark, tone: 'warning' },
  info: { icon: Info, tone: 'info' },
}

const projectName = (cwd: string) => cwd.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || cwd

/** One numbered choice per row and a free answer as the last row; answered, it folds to one line. */
export function QuestionCard({ question }: { question: Question }) {
  const workspace = useWorkspace()
  const [text, setText] = useState('')
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  if (question.answer) return <View className="min-h-9 flex-row items-start gap-2 py-2">
    <Icon as={CircleCheck} className="mt-0.5 size-4 text-success" />
    <Text className="min-w-0 flex-1 text-meta text-muted-foreground"><Text className="text-meta text-muted-foreground" numberOfLines={1}>{question.text}</Text>{' · '}<Text className="text-meta">{question.answer.text}</Text></Text>
  </View>

  const send = async (key: string, value: string) => {
    setBusy(key)
    setError('')
    try {
      await workspace.actions.answer(question, value)
      void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success)
      // Stays busy until the answer arrives and folds or removes the card, so it never flashes back to open.
    } catch (caught) {
      setError(humanError(caught instanceof Error ? caught.message : '', undefined, '这次回答'))
      setBusy(null)
    }
  }
  const typed = () => {
    const value = text.trim()
    const numbered = /^[1-9]$/.test(value) ? question.choices[Number(value) - 1] : undefined
    void (numbered ? send(`choice:${numbered.label}`, numbered.label) : send('text', value))
  }
  const device = workspace.members.find((member) => member.device_id === question.about.node)?.name ?? '另一台设备'
  const { icon, tone } = looks[question.severity]
  return <ActionCard icon={icon} tone={tone} title={question.text}>
    <View className="flex-row flex-wrap items-center gap-x-3 gap-y-1">
      {question.about.agent ? <Meta icon={providerIcons[question.about.agent]} label="来自">{providerNames[question.about.agent]}</Meta> : null}
      {question.about.cwd ? <Meta icon={FolderOpen} label="项目">{projectName(question.about.cwd)}</Meta> : null}
      <Meta icon={Laptop} label="设备">{device}</Meta>
    </View>
    <View accessibilityRole="radiogroup" accessibilityLabel="选一个回答" className="gap-2">
      {question.choices.map((choice, index) => {
        const key = `choice:${choice.label}`
        return <Pressable key={choice.label} accessibilityRole="button" disabled={busy !== null} onPress={() => void send(key, choice.label)}
          className={cn(row, 'active:bg-muted', busy !== null && 'opacity-50')}>
          <View className={cn(badge, 'bg-muted')}>
            {busy === key ? <Icon as={LoaderCircle} className="size-4 text-muted-foreground" /> : <Text className="text-meta font-semibold text-muted-foreground">{index + 1}</Text>}
          </View>
          <Text className="min-w-0 flex-1 font-medium">{choice.label}</Text>
        </Pressable>
      })}
      <View className={cn(row, 'pe-1', busy !== null && 'opacity-50')}>
        <View className={cn(badge, 'bg-muted')}><Icon as={PencilLine} className="size-4 text-muted-foreground" /></View>
        <TextInput {...caret} maxFontSizeMultiplier={2} accessibilityLabel="其他回答" placeholder={question.choices.length ? '其他回答' : '输入回答'} placeholderTextColorClassName="accent-muted-foreground"
          className="min-h-11 min-w-0 flex-1 font-sans text-body text-foreground" value={text} maxLength={4000} editable={busy === null}
          returnKeyType="send" submitBehavior="blurAndSubmit" onChangeText={setText} onSubmitEditing={() => { if (text.trim()) typed() }} />
        <Button variant="ghost" size="icon" accessibilityLabel="发送回答" disabled={busy !== null || !text.trim()} onPress={typed}>
          <Icon as={busy === 'text' ? LoaderCircle : ArrowUp} />
        </Button>
      </View>
    </View>
    <ErrorText error={error} />
  </ActionCard>
}

const row = 'min-h-12 flex-row items-center gap-3 rounded-lg border border-border bg-card px-3 py-2'
const badge = 'min-h-7 min-w-7 items-center justify-center rounded-md'

function Meta({ icon, label, children }: { icon: LucideIcon; label: string; children: string }) {
  return <View accessible accessibilityLabel={`${label} ${children}`} className="min-w-0 flex-row items-center gap-1">
    <Icon as={icon} className="size-4 text-muted-foreground" />
    <Text numberOfLines={1} className="text-meta text-muted-foreground">{children}</Text>
  </View>
}
