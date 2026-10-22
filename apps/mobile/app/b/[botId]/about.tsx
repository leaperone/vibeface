import { Stack, useLocalSearchParams } from 'expo-router'
import { AlarmClock, BookOpen, Bot, CalendarClock, CirclePause, NotebookPen, Pause, Play, Repeat, ScrollText, Star, Trash2, type LucideIcon } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { ScrollView, View } from 'react-native'
import { formatWhen, scheduleDetail, scheduleFrequency } from '@vibeface/client/workspace/bot-settings.ts'
import { ActionButton } from '@/components/action'
import { EmptyState } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'

/** A bot as the web's bot panel shows it: primary or not, what it does, its schedules, notes and the shared skills. Changes are made by asking it in the chat. */
export default function BotAbout() {
  const { botId } = useLocalSearchParams<{ botId: string }>()
  const shared = useLive()
  const bot = shared?.bots.find((item) => item.bot_id === botId)
  if (!shared || !bot) return <EmptyState icon={Bot} title="没有这个 bot" />
  const view = shared.conversationId === bot.conversation_id ? shared.data?.bot_settings : undefined
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="gap-8 p-4 pb-10">
    <Stack.Screen options={{ title: bot.name }} />
    <View className="gap-3">
      <Text className="text-muted-foreground">在聊天里说一声就能改</Text>
      {shared.primary === bot.conversation_id
        ? <View accessible accessibilityHint={primaryHint} className="flex-row items-center gap-1.5"><Icon as={Star} className="size-4 fill-muted-foreground text-muted-foreground" /><Text className="text-muted-foreground">主 bot</Text></View>
        : <ActionButton variant="outline" className="h-11 self-start" icon={Star} accessibilityHint={primaryHint} disabled={!shared.writable} action={() => shared.makePrimary(bot.conversation_id)}>设为主 bot</ActionButton>}
    </View>
    <Section icon={ScrollText} title="说明">
      {bot.instructions.trim() ? <Text selectable>{bot.instructions}</Text> : <Empty>还没有说明</Empty>}
    </Section>
    {view ? <>
      <Section icon={CalendarClock} title="定时任务" count={view.schedules.length}>
        {view.schedules.length ? view.schedules.map((schedule) => <View key={schedule.schedule_id} className="flex-row items-start gap-3">
          <Icon as={schedule.every_minutes === undefined ? AlarmClock : Repeat} className="mt-0.5 size-4 text-muted-foreground" />
          <View className="min-w-0 flex-1 gap-0.5">
            <Text className="font-medium">{schedule.name}</Text>
            <View className="flex-row flex-wrap items-center gap-x-2">
              {!schedule.enabled ? <View className="flex-row items-center gap-1"><Icon as={CirclePause} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">已暂停</Text></View>
                : schedule.next_run_at_ms !== null ? <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{formatWhen(schedule.next_run_at_ms)}</Text> : null}
              <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{scheduleFrequency(schedule)}</Text>
            </View>
            <Text numberOfLines={2} className="text-meta text-muted-foreground">{scheduleDetail(schedule) || schedule.prompt}</Text>
            <View className="flex-row gap-2 pt-1">
              <ActionButton variant="outline" size="sm" className="h-9" icon={schedule.enabled ? Pause : Play} disabled={!shared.writable} action={() => shared.controlSchedule(schedule.schedule_id, schedule.enabled ? 'pause' : 'resume')}>{schedule.enabled ? '暂停' : '恢复'}</ActionButton>
              <ActionButton variant="outline" size="sm" className="h-9" icon={Trash2} disabled={!shared.writable} action={() => shared.controlSchedule(schedule.schedule_id, 'delete')}>删除</ActionButton>
            </View>
          </View>
        </View>) : <Empty>还没有定时任务</Empty>}
      </Section>
      <Section icon={NotebookPen} title="笔记" count={view.memory.length}>
        {view.memory.length ? view.memory.map((note) => <Text key={note.memory_id} selectable numberOfLines={2} className="text-meta">{note.text}</Text>) : <Empty>还没有笔记</Empty>}
      </Section>
      <Section icon={BookOpen} title="共用 skill" count={view.skills.length}>
        {view.skills.length ? view.skills.map((skill) => <View key={skill.skill_id} className="gap-0.5">
          <Text className="font-medium">{skill.name}</Text>
          {skill.description ? <Text className="text-meta text-muted-foreground">{skill.description}</Text> : null}
        </View>) : <Empty>还没有 skill</Empty>}
      </Section>
    </> : null}
  </ScrollView>
}

const primaryHint = '没有 bot 负责的 Codex 会话，由主 bot 分派'

function Section({ icon, title, count, children }: { icon: LucideIcon; title: string; count?: number; children: ReactNode }) {
  return <View className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} className="size-4 text-muted-foreground" />
      <Text role="heading" className="text-meta font-medium text-muted-foreground">{title}</Text>
      {count ? <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text> : null}
    </View>
    {children}
  </View>
}

function Empty({ children }: { children: string }) {
  return <Text className="text-meta text-muted-foreground">{children}</Text>
}
