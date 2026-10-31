import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { humanError } from '@vibeface/client/lib/errors.ts'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { answerLabel, attentionLabel, approvalTask, declined, questionMeta, questionParts, settledAnswer, sharedProject } from '@vibeface/client/workspace/questions.ts'
import type { AnswerResult } from '@vibeface/client/workspace/types.ts'
import { ArrowUp, Circle, CircleAlert, CircleCheck, CircleDot, CircleSlash, CircleX, FolderOpen, Hand, Info, ListChecks, LoaderCircle, MessageCircleQuestionMark, Laptop, RotateCw, TriangleAlert, type LucideIcon } from 'lucide-react-native'
import { useId, useRef, useState, type ComponentRef } from 'react'
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

type Selection = { kind: 'none' } | { kind: 'choice'; label: string } | { kind: 'custom' }

/**
 * Select or write an answer locally, then send it, as on the web.
 * Answered or closed, it shows the result. `answer`: the chat's message answering it, shown here instead of as a bubble.
 * `inTask`: inside the task's own page, which already names the task, device and folder.
 */
export function QuestionCard({ question, answer, inTask = false }: { question: Question; answer?: string; inTask?: boolean }) {
  const workspace = useWorkspace()
  const shared = useLive()
  const { ready } = useDevice()
  const fieldId = useId()
  const [text, setText] = useState('')
  const [selection, setSelection] = useState<Selection>(question.choices.length ? { kind: 'none' } : { kind: 'custom' })
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [result, setResult] = useState<AnswerResult | null>(null)
  const running = useRef(false)
  const attempted = useRef('')
  const textRef = useRef<ComponentRef<typeof TextInput>>(null)
  const id = question.question_id
  const standing = question.answer?.text ?? answer
  const settled = settledAnswer(standing, shared?.answered[id] ?? result?.text, question.answer?.author, ready?.principal.device_id)
  const raced = result?.status === 'already_answered' && settled?.text === result.text && result.text !== attempted.current
  const elsewhere = settled?.elsewhere || raced
  const warned = settled?.beaten || raced
  const { ask, detail } = questionParts(question.text)
  const member = workspace.members.find((item) => item.device_id === question.about.node)
  // Read back later, an answered approval says which task on which device it settled.
  const heldTask = inTask ? null : approvalTask(question, [...shared?.openWorks ?? [], ...shared?.data?.works ?? []])
  const about = heldTask ? `${heldTask} · ${member?.name ?? '另一台设备'}` : undefined
  if (settled) return <Settled icon={warned ? CircleAlert : declined(settled.text) ? CircleX : CircleCheck} className={warned ? 'text-warning' : declined(settled.text) ? 'text-danger' : 'text-success'} ask={ask} detail={detail} about={about}>{elsewhere ? `已由另一端回答：${settled.text}` : answerLabel(settled.text)}</Settled>
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
  const submit = () => {
    if (disabled || running.current) return
    if (selection.kind === 'none') { setError('选一个回答，或自己写回答。'); return }
    if (selection.kind === 'custom' && !text.trim()) { setError('写下你的回答。'); textRef.current?.focus(); return }
    void send('answer', selection.kind === 'choice' ? selection.label : text)
  }
  const { approval, task, local } = questionMeta(question, shared?.openWorks ?? [], workspace.bots, ready?.entry.device_id)
  const project = question.about.cwd ? sharedProject(question.about.cwd, question.about.node, shared?.nodes ?? []) : null
  const { icon, tone } = looks[question.severity]
  return <ActionCard icon={approval ? Hand : icon} tone={tone} title={ask}>
    <Text className="text-meta text-muted-foreground">{attentionLabel(question)}{question.attention?.device_id ? ` · 处理设备 ${workspace.members.find((item) => item.device_id === question.attention?.device_id)?.name ?? '运行设备'}` : ''}</Text>
    {question.attention?.kind === 'browser_takeover' ? <Text className="text-meta text-muted-foreground">请在运行设备的浏览器中完成操作，确认结果后再回来回答。</Text> : null}
    {detail ? <Text className="text-body">{detail}</Text> : null}
    {inTask ? null : <View className="flex-row flex-wrap items-center gap-x-3 gap-y-1">
      {task ? <Link href={`/b/${task.bot_id}/work/${task.work_id}`} asChild>
        <Pressable accessibilityRole="link" accessibilityLabel={`任务 ${task.title}`} hitSlop={8} className="min-w-0 flex-row items-center gap-1 active:opacity-60">
          <Icon as={ListChecks} className="size-4 text-muted-foreground" />
          <Text numberOfLines={1} className="text-meta text-muted-foreground underline">{task.title}</Text>
        </Pressable>
      </Link> : null}
      {question.about.agent ? <Meta icon={providerIcons[question.about.agent]} label="来自">{providerNames[question.about.agent]}</Meta> : null}
      {project ? <Meta icon={FolderOpen} label="项目">{project.name}</Meta> : null}
      {local ? null : <Meta icon={member ? deviceIcon(member) : Laptop} label="设备">{member?.name ?? '另一台设备'}</Meta>}
    </View>}
    {pending ? <View role="status" className="gap-2">
      <Text className="text-body">{pending.text}</Text>
      <Text className="text-meta text-muted-foreground">{busy !== null ? '正在发送' : '没确认送达'}</Text>
      <Button variant="outline" className="min-h-11 self-start" disabled={disabled} accessibilityState={{ disabled, busy: busy === 'retry' }} onPress={() => void send('retry', pending.text)}>
        <Icon as={busy === 'retry' ? LoaderCircle : RotateCw} /><Text>再试</Text>
      </Button>
    </View> : <View className="gap-2">
      {question.choices.length ? <View accessibilityRole="radiogroup" accessibilityLabel="选一个回答" className="gap-2">
        {question.choices.map((choice, index) => {
          const checked = selection.kind === 'choice' && selection.label === choice.label
          return <Pressable key={index} accessibilityRole="radio" accessibilityLabel={choice.label} accessibilityState={{ checked, disabled }} disabled={disabled} onPress={() => { setSelection({ kind: 'choice', label: choice.label }); setError('') }}
            className={cn(row, 'active:bg-muted', checked && 'border-ring', disabled && 'opacity-50')}>
            <Icon as={checked ? CircleDot : Circle} className="mt-0.5 size-4 text-muted-foreground" />
            <Text className="min-w-0 flex-1 font-medium">{choice.label}</Text>
          </Pressable>
        })}
        <Pressable accessibilityRole="radio" accessibilityLabel="自己写回答" accessibilityState={{ checked: selection.kind === 'custom', disabled }} disabled={disabled} onPress={() => { setSelection({ kind: 'custom' }); setError('') }} className={cn(row, 'active:bg-muted', selection.kind === 'custom' && 'border-ring', disabled && 'opacity-50')}>
          <Icon as={selection.kind === 'custom' ? CircleDot : Circle} className="mt-0.5 size-4 text-muted-foreground" /><Text>自己写回答</Text>
        </Pressable>
      </View> : null}
      {selection.kind === 'custom' ? <View className="gap-1.5">
        <Text nativeID={`${fieldId}-label`} className="text-meta font-medium">你的回答</Text>
        <TextInput ref={textRef} {...caret} maxFontSizeMultiplier={2} accessibilityLabel="你的回答" accessibilityLabelledBy={`${fieldId}-label`} accessibilityHint={error || '发送会回答本题，让 Bot 按回答继续处理。'} placeholder="写下你的选择、条件或需要澄清的问题" placeholderTextColorClassName="accent-muted-foreground"
          className="min-h-24 rounded-lg border border-border bg-card p-3 font-sans text-body text-foreground" value={text} maxLength={4000} editable={!disabled}
          multiline textAlignVertical="top" submitBehavior="newline" onChangeText={(value) => { setText(value); setError('') }} />
      </View> : null}
      <Text className="text-meta text-muted-foreground">发送会回答本题，让 Bot 按回答继续处理。</Text>
      <Button variant={selection.kind === 'none' ? 'outline' : 'default'} className="min-h-11 self-start" accessibilityLabel="发送回答" disabled={disabled} accessibilityState={{ disabled, busy: busy === 'answer' }} onPress={submit}>
        <Icon as={busy === 'answer' ? LoaderCircle : ArrowUp} /><Text>发送回答</Text>
      </Button>
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
