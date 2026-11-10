import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { humanError } from '@vibeface/client/lib/errors.ts'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, questionParts, settledAnswer, sharedProject } from '@vibeface/client/workspace/questions.ts'
import type { AnswerResult } from '@vibeface/client/workspace/types.ts'
import { ArrowUp, CircleAlert, CircleCheck, CircleSlash, CircleX, FolderOpen, Hand, Info, ListChecks, LoaderCircle, MessageCircleQuestionMark, Laptop, RotateCw, TriangleAlert, X, type LucideIcon } from 'lucide-react-native'
import { useRef, useState } from 'react'
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
 * As on the web, a choice sends on tap, the last line takes a written answer, and 忽略 sets it aside.
 * Answered or closed, it shows the result. `answer`: the chat's message answering it, shown here instead of as a bubble.
 * `inTask`: inside the task's own page, which already names the task, device and folder.
 */
export function QuestionCard({ question, answer, inTask = false }: { question: Question; answer?: string; inTask?: boolean }) {
  const workspace = useWorkspace()
  const shared = useLive()
  const { ready } = useDevice()
  const [text, setText] = useState('')
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [result, setResult] = useState<AnswerResult | null>(null)
  const running = useRef(false)
  const attempted = useRef('')
  const id = question.question_id
  const inputTarget = question.target
  const taskScope = inputTarget?.kind === 'task' || inputTarget?.kind === 'channel_task' ? inputTarget.task_id : null
  const standing = question.answer?.text ?? answer
  const settled = settledAnswer(standing, shared?.answered[id] ?? result?.text, question.answer?.author, ready?.principal.device_id)
  const displayAnswer = (value: string) => { const choice = question.choices.find((choice) => choice.label === value); return choice?.action === 'allow_once' ? '允许本次' : choice?.action === 'deny' ? '拒绝' : value }
  const approvalChoices = question.choices.filter((choice) => choice.action === 'allow_once' || choice.action === 'deny')
  // The Node takes only an approval's own actions for a live request; 拒绝 clears it there.
  const ignorable = !approvalChoices.length && !(question.attention?.kind === 'approval' && question.about.request_id)
  const raced = result?.status === 'already_answered' && settled?.text === result.text && result.text !== attempted.current
  const elsewhere = settled?.elsewhere || raced
  const warned = settled?.beaten || raced
  const { ask, detail } = questionParts(question.text)
  const member = workspace.members.find((item) => item.device_id === question.about.node)
  // Read back later, an answered approval says which task on which device it settled.
  const heldTask = inTask ? null : approvalTask(question, [...shared?.openWorks ?? [], ...shared?.data?.works ?? []])
  const about = heldTask ? `${heldTask} · ${member?.name ?? '另一台设备'}` : undefined
  if (settled) return <Settled icon={warned ? CircleAlert : settled.text.trim() === ignoredAnswer ? CircleSlash : declined(displayAnswer(settled.text)) ? CircleX : CircleCheck} className={warned ? 'text-warning' : settled.text.trim() === ignoredAnswer ? undefined : declined(displayAnswer(settled.text)) ? 'text-danger' : 'text-success'} ask={ask} detail={detail} about={about}>{elsewhere ? `已由另一端回答：${displayAnswer(settled.text)}` : answerLabel(displayAnswer(settled.text))}</Settled>
  if (question.closed) return <Settled icon={CircleSlash} ask={ask} detail={detail} about={about}>已失效</Settled>

  const pending = shared?.pendingAnswers.find((item) => item.question_id === id)
  const disabled = !shared?.writable || busy !== null
  const send = async (key: string, value: string) => {
    if (disabled || running.current || pending && value !== pending.text) return
    running.current = true
    attempted.current = value.trim()
    setBusy(key)
    setError('')
    try {
      setResult(await workspace.actions.answer(question, value))
      void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success)
    } catch (caught) {
      setError(humanError(caught instanceof Error ? caught.message : '', undefined, '这次回答'))
    } finally {
      running.current = false
      setBusy(null)
    }
  }
  const { approval, task, local } = questionMeta(question, shared?.openWorks ?? [], workspace.bots, ready?.entry.device_id)
  const project = question.about.cwd ? sharedProject(question.about.cwd, question.about.node, shared?.nodes ?? []) : null
  const { icon, tone } = looks[question.severity]
  return <ActionCard icon={approval ? Hand : icon} tone={tone} title={ask}>
    <Text className="text-meta text-muted-foreground">{attentionLabel(question)}{question.attention?.device_id ? ` · 处理设备 ${workspace.members.find((item) => item.device_id === question.attention?.device_id)?.name ?? '运行设备'}` : ''}</Text>
    {question.attention?.kind === 'browser_takeover' ? <Text className="text-meta text-muted-foreground">请在运行设备的浏览器中完成操作，确认结果后再回来回答。</Text> : null}
    {detail ? <Text className="text-body">{detail}</Text> : null}
    {inTask ? null : <View className="flex-row flex-wrap items-center gap-x-3 gap-y-1">
      {taskScope ? <Link href={`/task/${taskScope}`} asChild><Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-1"><Icon as={ListChecks} className="size-4 text-muted-foreground" /><Text className="text-meta underline">打开所属任务</Text></Pressable></Link> : null}
      {task ? <Link href={`/b/${task.bot_id}/work/${task.work_id}`} asChild>
        <Pressable accessibilityRole="link" accessibilityLabel={`任务 ${task.title}`} className="min-h-11 min-w-0 flex-row items-center gap-1 active:opacity-60">
          <Icon as={ListChecks} className="size-4 text-muted-foreground" />
          <Text numberOfLines={1} className="text-meta text-muted-foreground underline">{task.title}</Text>
        </Pressable>
      </Link> : null}
      {question.about.agent ? <Meta icon={providerIcons[question.about.agent]} label="来自">{providerNames[question.about.agent]}</Meta> : null}
      {project ? <Meta icon={FolderOpen} label="项目">{project.name}</Meta> : null}
      {local ? null : <Meta icon={member ? deviceIcon(member) : Laptop} label="设备">{member?.name ?? '另一台设备'}</Meta>}
    </View>}
    {pending ? <View role="status" className="gap-2">
      <Text className="text-body">{displayAnswer(pending.text)}</Text>
      <Text className="text-meta text-muted-foreground">{busy !== null ? '正在发送' : '没确认送达'}</Text>
      <Button variant="outline" className="min-h-11 self-start" disabled={disabled} accessibilityState={{ disabled, busy: busy === 'retry' }} onPress={() => void send('retry', pending.text)}>
        <Icon as={busy === 'retry' ? LoaderCircle : RotateCw} /><Text>再试</Text>
      </Button>
    </View> : <View className="gap-2">
      {approvalChoices.length ? <View className="flex-row flex-wrap gap-2">{approvalChoices.map((choice) => <Button key={choice.label} variant={choice.action === 'allow_once' ? 'default' : 'outline'} className="min-h-11" accessibilityLabel={choice.action === 'allow_once' ? '允许本次' : '拒绝'} disabled={disabled} accessibilityState={{ disabled, busy: busy === `permission:${choice.action}` }} onPress={() => void send(`permission:${choice.action}`, choice.label)}><Icon as={busy === `permission:${choice.action}` ? LoaderCircle : choice.action === 'allow_once' ? CircleCheck : CircleX} /><Text>{choice.action === 'allow_once' ? '允许本次' : '拒绝'}</Text></Button>)}</View> : <>
        {question.choices.map((choice, index) => <Pressable key={index} accessibilityRole="button" accessibilityLabel={choice.label} accessibilityState={{ disabled, busy: busy === `choice:${index}` }} disabled={disabled} onPress={() => void send(`choice:${index}`, choice.label)}
          className={cn(row, 'active:bg-muted', disabled && 'opacity-50')}>
          {busy === `choice:${index}` ? <Icon as={LoaderCircle} className="mt-0.5 size-4 text-muted-foreground" /> : <Text className="w-4 text-center text-muted-foreground">{index + 1}</Text>}
          <Text className="min-w-0 flex-1 font-medium">{choice.label}</Text>
        </Pressable>)}
        <View className="flex-row items-end gap-2">
          <TextInput {...caret} maxFontSizeMultiplier={2} accessibilityLabel="你的回答" placeholder={question.choices.length ? '或者自己写回答' : '写下你的回答'} placeholderTextColorClassName="accent-muted-foreground"
            className="max-h-40 min-h-11 flex-1 rounded-lg border border-border bg-card px-3 py-2.5 font-sans text-body text-foreground" value={text} maxLength={4000} editable={!disabled}
            multiline textAlignVertical="top" submitBehavior="newline" onChangeText={(value) => { setText(value); setError('') }} />
          <Button size="icon" className="size-11" accessibilityLabel="发送回答" disabled={disabled || !text.trim()} accessibilityState={{ disabled: disabled || !text.trim(), busy: busy === 'answer' }} onPress={() => void send('answer', text)}>
            <Icon as={busy === 'answer' ? LoaderCircle : ArrowUp} />
          </Button>
        </View>
      </>}
      {ignorable ? <Button variant="ghost" className="min-h-11 self-start" accessibilityLabel="忽略" disabled={disabled} accessibilityState={{ disabled, busy: busy === 'ignore' }} onPress={() => void send('ignore', ignoredAnswer)}>
        <Icon as={busy === 'ignore' ? LoaderCircle : X} className="text-muted-foreground" /><Text className="text-muted-foreground">忽略</Text>
      </Button> : null}
    </View>}
    <ErrorText error={error} />
  </ActionCard>
}

const row = 'min-h-12 flex-row items-start gap-3 rounded-lg border border-border bg-card px-3 py-3'

function Settled({ icon, className, ask, detail, about, children }: { icon: LucideIcon; className?: string; ask: string; detail: string; about?: string; children: string }) {
  return <View role="status" className="min-h-9 flex-row items-start gap-2 py-2">
    <Icon as={icon} className={cn('mt-0.5 size-4 text-muted-foreground', className)} />
    <View className="min-w-0 flex-1 gap-0.5">
      {about ? <View className="min-w-0 flex-row items-center gap-1"><Icon as={ListChecks} className="size-3.5 text-muted-foreground" /><Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{about}</Text></View> : null}
      <Text className="text-meta text-muted-foreground"><Text className="text-meta text-muted-foreground" numberOfLines={1}>{ask}</Text>{' · '}<Text className="text-meta">{children}</Text></Text>
      {detail ? <Text className="text-meta text-muted-foreground">{detail}</Text> : null}
    </View>
  </View>
}

function Meta({ icon, label, children }: { icon: LucideIcon; label: string; children: string }) {
  return <View accessible accessibilityLabel={`${label} ${children}`} className="min-w-0 flex-row items-center gap-1">
    <Icon as={icon} className="size-4 text-muted-foreground" />
    <Text numberOfLines={1} className="text-meta text-muted-foreground">{children}</Text>
  </View>
}
