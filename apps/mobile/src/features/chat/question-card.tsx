import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { humanError } from '@vibeface/client/lib/errors.ts'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { answerLabel, declined, questionMeta, sharedProject } from '@vibeface/client/workspace/questions.ts'
import { ArrowUp, CircleCheck, CircleSlash, CircleX, FolderOpen, Hand, Info, ListChecks, LoaderCircle, MessageCircleQuestionMark, PencilLine, Server, TriangleAlert, type LucideIcon } from 'lucide-react-native'
import { useState } from 'react'
import { Pressable, TextInput, View } from 'react-native'
import { ErrorText } from '@/components/action'
import { ActionCard, providerIcons, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { caret } from '@/components/ui/input'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import { deviceIcon } from '@/features/devices/device-icon'
import type { Question } from '@/data/types'
import { cn } from '@/lib/utils'

const looks: Record<Question['severity'], { icon: LucideIcon; tone: Tone }> = {
  urgent: { icon: TriangleAlert, tone: 'warning' },
  attention: { icon: MessageCircleQuestionMark, tone: 'warning' },
  info: { icon: Info, tone: 'info' },
}

/**
 * One numbered choice per row and a free answer as the last row, as on the web; a request to allow something only takes its choices.
 * Answered or closed, it folds to one line. `answer`: the chat's message answering it, which shows here instead of as a bubble.
 * `inTask`: inside the task's own page, which already names the task, device and folder.
 */
export function QuestionCard({ question, answer, inTask = false }: { question: Question; answer?: string; inTask?: boolean }) {
  const workspace = useWorkspace()
  const shared = useLive()
  const { ready } = useDevice()
  const [text, setText] = useState('')
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const settled = question.answer?.text ?? answer
  if (settled !== undefined) return <Settled icon={declined(settled) ? CircleX : CircleCheck} className={declined(settled) ? 'text-danger' : 'text-success'} ask={question.text}>{declined(settled) ? answerLabel(settled) : settled}</Settled>
  if (question.closed) return <Settled icon={CircleSlash} ask={question.text}>已失效</Settled>

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
  const { approval, task, local } = questionMeta(question, shared?.openWorks ?? [], workspace.bots, ready?.entry.device_id)
  const member = workspace.members.find((item) => item.device_id === question.about.node)
  const project = question.about.cwd ? sharedProject(question.about.cwd, question.about.node, shared?.nodes ?? []) : null
  const { icon, tone } = looks[question.severity]
  return <ActionCard icon={approval ? Hand : icon} tone={tone} title={question.text}>
    {inTask ? null : <View className="flex-row flex-wrap items-center gap-x-3 gap-y-1">
      {task ? <Link href={`/b/${task.bot_id}/work/${task.work_id}`} asChild>
        <Pressable accessibilityRole="link" accessibilityLabel={`任务 ${task.title}`} hitSlop={8} className="min-w-0 flex-row items-center gap-1 active:opacity-60">
          <Icon as={ListChecks} className="size-4 text-muted-foreground" />
          <Text numberOfLines={1} className="text-meta text-muted-foreground underline">{task.title}</Text>
        </Pressable>
      </Link> : null}
      {question.about.agent ? <Meta icon={providerIcons[question.about.agent]} label="来自">{providerNames[question.about.agent]}</Meta> : null}
      {project ? <Meta icon={FolderOpen} label="项目">{project.name}</Meta> : null}
      {local ? null : <Meta icon={member ? deviceIcon(member) : Server} label="设备">{member?.name ?? '另一台设备'}</Meta>}
    </View>}
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

function Settled({ icon, className, ask, children }: { icon: LucideIcon; className?: string; ask: string; children: string }) {
  return <View className="min-h-9 flex-row items-start gap-2 py-2">
    <Icon as={icon} className={cn('mt-0.5 size-4 text-muted-foreground', className)} />
    <Text className="min-w-0 flex-1 text-meta text-muted-foreground"><Text className="text-meta text-muted-foreground" numberOfLines={1}>{ask}</Text>{' · '}<Text className="text-meta">{children}</Text></Text>
  </View>
}

function Meta({ icon, label, children }: { icon: LucideIcon; label: string; children: string }) {
  return <View accessible accessibilityLabel={`${label} ${children}`} className="min-w-0 flex-row items-center gap-1">
    <Icon as={icon} className="size-4 text-muted-foreground" />
    <Text numberOfLines={1} className="text-meta text-muted-foreground">{children}</Text>
  </View>
}
