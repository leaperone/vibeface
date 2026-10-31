import { Stack, useLocalSearchParams } from 'expo-router'
import { AlarmClock, BookOpen, Bot, CalendarClock, Check, CirclePause, Cpu, FolderOpen, Inbox, Laptop, MessagesSquare, NotebookPen, Pause, Play, Repeat, ScrollText, Star, Trash2, WifiOff, type LucideIcon } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Alert, Pressable, ScrollView, View } from 'react-native'
import { botCopy, folderLabels, formatIn, projectChoices, projectKey, projectPlace, scheduleDetail, scheduleFrequency, sortSchedules } from '@vibeface/client/workspace/bot-settings.ts'
import { asProvider, messageOf, providerNames, type ProviderId } from '@vibeface/client/lib/node.ts'
import type { BotProject, BotRecord } from '@vibeface/client/workspace/types.ts'
import { useAvatarPick } from '@vibeface/client/avatar/pick.ts'
import { AvatarPicker } from '@/components/avatar-picker'
import { cn } from '@/lib/utils'
import { ActionButton } from '@/components/action'
import { botMood, BotFace } from '@/components/bot-face'
import { Details, EmptyState, providerIcons, SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@/data/context'
import { nodeName } from '@vibeface/client/workspace/labels.ts'
import { useLive } from '@/data/live'

/**
 * Bot settings and a read-only entry to its shared context. The face and project are picked here; other settings change in chat.
 */
export default function BotAbout() {
  const { botId } = useLocalSearchParams<{ botId: string }>()
  const shared = useLive()
  const bot = shared?.bots.find((item) => item.bot_id === botId)
  if (!shared || !bot) return <EmptyState icon={Bot} title={botCopy.notFound} className="flex-1 justify-center" />
  return <About bot={bot} shared={shared} />
}

function About({ bot, shared }: { bot: BotRecord; shared: NonNullable<ReturnType<typeof useLive>> }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const look = useAvatarPick(bot, (version, avatar) => shared.setBotAvatar(bot.conversation_id, version, avatar), (error) => Alert.alert('没换成', messageOf(error)))
  const view = shared.conversationId === bot.conversation_id ? shared.data?.bot_settings : undefined
  const schedules = sortSchedules(view?.schedules ?? [])
  const empty = view && !bot.project && !bot.instructions.trim() && !schedules.length && !view.memory.length && !view.skills.length
  const lookup = { nodes: shared.nodes, directory }
  const choices = projectChoices(shared.nodes, (node, index) => nodeName(node.device_id, lookup, `设备 ${index + 1}`))
  const projectNow = bot.project ? projectKey(bot.project) : ''
  const owner = shared.conversationId === bot.conversation_id ? shared.data?.conversation?.owner : undefined
  const ownerNode = shared.nodes.find((node) => node.device_id === owner)
  // The provider the bot runs on now, with its model; without one known, every model the bot chose.
  const running = asProvider(ownerNode?.capabilities?.manager)
  const models = running ? [[running, bot.models[running] ?? '']] as [ProviderId, string][] : Object.entries(bot.models) as [ProviderId, string][]
  const place = bot.project ? projectPlace(bot.project, owner, (id) => nodeName(id, lookup)) : ''
  const options: { key: string; project: BotProject | null; label: string }[] = [{ key: '', project: null, label: '不选' },
    ...(bot.project && !choices.some((item) => item.key === projectNow) ? [{ key: projectNow, project: bot.project, label: place }] : []), ...choices]
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Stack.Screen options={{ title: bot.name }} />
    <View className="flex-row items-center gap-3">
      <BotFace bot={{ bot_id: bot.bot_id, avatar: look.avatar }} mood={botMood(bot, workspace, shared)} size={48} />
      <View className="min-w-0 flex-1 items-start gap-1">
        {shared.primary === bot.conversation_id
          ? <View accessible accessibilityHint={botCopy.primaryHint} className="flex-row items-center gap-1"><Icon as={Star} className="size-3.5 fill-muted-foreground text-muted-foreground" /><Text className="text-meta text-muted-foreground">主 Bot</Text></View>
          : <ActionButton variant="outline" size="sm" icon={Star} accessibilityHint={botCopy.primaryHint} disabled={!shared.writable} action={() => shared.makePrimary(bot.conversation_id)}>设为主 Bot</ActionButton>}
        <Text className="text-meta text-muted-foreground">在聊天里说一声就能改</Text>
      </View>
    </View>
    <SettingsGroup><SettingsRow icon={MessagesSquare} label="上下文和协作" description="共享摘要、外部会话归属和调度观察" href={`/b/${bot.bot_id}/context`} /></SettingsGroup>
    {owner || models.length ? <SettingsGroup>
      {owner ? <SettingsRow icon={Laptop} label="运行设备">
        {ownerNode && !ownerNode.online ? <Icon as={WifiOff} accessibilityLabel="离线" className="size-4 text-warning" /> : null}
        <Text numberOfLines={1} className="max-w-[50%] text-muted-foreground">{nodeName(owner, lookup)}</Text>
      </SettingsRow> : null}
      {models.length ? models.map(([provider, model]) => <SettingsRow key={provider} icon={providerIcons[provider]} label={providerNames[provider]}>
        <Text numberOfLines={1} className={cn('max-w-[50%] text-muted-foreground', model && 'font-mono text-meta')}>{model || '设备默认'}</Text>
      </SettingsRow>) : <SettingsRow icon={Cpu} label="模型"><Text className="text-muted-foreground">设备默认</Text></SettingsRow>}
    </SettingsGroup> : null}
    {empty ? <View className="flex-row items-center gap-2"><Icon as={Inbox} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{botCopy.empty}</Text></View> : null}
    {bot.project || choices.length ? <Section icon={FolderOpen} title={folderLabels.project}>
      {shared.writable && choices.length ? <View accessibilityRole="radiogroup" className="overflow-hidden rounded-xl border border-border">
        {options.map((item, index) => {
          const selected = item.key === projectNow
          return <Pressable key={item.key || 'none'} accessibilityRole="radio" accessibilityState={{ checked: selected }} className={cn('min-h-12 flex-row items-center gap-3 px-4 py-2 active:bg-muted', index > 0 && 'border-t border-border')}
            onPress={() => { if (!selected) void shared.setBotProject(bot.conversation_id, item.project).catch((error: unknown) => Alert.alert('没换成', messageOf(error))) }}>
            <Text numberOfLines={1} className="min-w-0 flex-1">{item.label}</Text>
            {selected ? <Icon as={Check} className="size-4" /> : null}
          </Pressable>
        })}
      </View> : <Text selectable>{place}</Text>}
    </Section> : null}
    <Details label="换长相">
      <AvatarPicker botId={bot.bot_id} avatar={look.avatar} face={look.face} disabled={!shared.writable} busy={look.pending} pick={look.pick} />
    </Details>
    {bot.instructions.trim() ? <Section icon={ScrollText} title="说明"><Text selectable>{bot.instructions}</Text></Section> : null}
    {schedules.length ? <Section icon={CalendarClock} title="定时任务" count={schedules.length}>
      {schedules.map((schedule) => <View key={schedule.schedule_id} className="flex-row items-start gap-3">
        <Icon as={schedule.every_minutes === undefined ? AlarmClock : Repeat} className="mt-0.5 size-4 text-muted-foreground" />
        <View className="min-w-0 flex-1 gap-0.5">
          <Text className="font-medium">{schedule.name}</Text>
          <View className="flex-row flex-wrap items-center gap-x-2">
            {!schedule.enabled ? <View className="flex-row items-center gap-1"><Icon as={CirclePause} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">已暂停</Text></View>
              : schedule.next_run_at_ms !== null ? <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{formatIn(schedule.next_run_at_ms)}</Text> : null}
            <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{scheduleFrequency(schedule)}</Text>
          </View>
          <Text numberOfLines={2} className="text-meta text-muted-foreground">{scheduleDetail(schedule) || schedule.prompt}</Text>
          <View className="flex-row gap-2 pt-1">
            <ActionButton variant="outline" size="sm" icon={schedule.enabled ? Pause : Play} disabled={!shared.writable} action={() => shared.controlSchedule(schedule.schedule_id, schedule.enabled ? 'pause' : 'resume')}>{schedule.enabled ? '暂停' : '恢复'}</ActionButton>
            <Button variant="outline" size="sm" disabled={!shared.writable} onPress={() => Alert.alert(`删除「${schedule.name}」？`, undefined, [
              { text: '取消', style: 'cancel' },
              { text: '删除', style: 'destructive', onPress: () => void shared.controlSchedule(schedule.schedule_id, 'delete').catch((error: unknown) => Alert.alert('删除失败', error instanceof Error ? error.message : String(error))) },
            ])}>
              <Icon as={Trash2} /><Text>删除</Text>
            </Button>
          </View>
        </View>
      </View>)}
    </Section> : null}
    {view?.memory.length ? <Section icon={NotebookPen} title="笔记" count={view.memory.length}>
      {view.memory.map((note) => <Text key={note.memory_id} selectable numberOfLines={2} className="text-meta">{note.text}</Text>)}
    </Section> : null}
    {view?.skills.length ? <Section icon={BookOpen} title={botCopy.skills(bot.skills, view.skills.length)}>
      {view.skills.map((skill) => <View key={skill.skill_id} className="gap-0.5">
        <Text className="font-medium">{skill.name}</Text>
        {skill.description ? <Text className="text-meta text-muted-foreground">{skill.description}</Text> : null}
      </View>)}
    </Section> : null}
  </ScrollView>
}

function Section({ icon, title, count, children }: { icon: LucideIcon; title: string; count?: number; children: ReactNode }) {
  return <View className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} />
      <Text role="heading" className="text-heading font-semibold">{title}</Text>
      {count ? <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text> : null}
    </View>
    {children}
  </View>
}
