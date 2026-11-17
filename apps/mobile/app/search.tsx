import { Link, Stack, useLocalSearchParams } from 'expo-router'
import { ChevronRight, Search } from 'lucide-react-native'
import { useEffect, useMemo, useRef, useState } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { searchMessages } from '@vibeface/client/workspace/history-search.ts'
import type { MessageSearchItem, MessageSearchScope } from '@vibeface/client/workspace/types.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { useWorkspace } from '@/data/context'
import { localConversations } from '@/data/cached'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Input } from '@/components/arc/input'
import { Text } from '@/components/arc/text'
import { EmptyState } from '@/components/kit'
import { ErrorText } from '@/components/action'

type Hit = Pick<MessageSearchItem, 'event_hash' | 'message_id' | 'conversation_id' | 'created_at_ms' | 'text' | 'rank'> & { source: 'local' | 'server' }
type SearchState = { key: string; items: Hit[]; cursor: string | null; complete: boolean } & ({ kind: 'idle' } | { kind: 'loading' } | { kind: 'ready' } | { kind: 'failed'; error: string })

export default function MessageSearch() {
  const params = useLocalSearchParams<{ bot?: string; task?: string; channel?: string }>()
  const workspace = useWorkspace(), device = useDevice()
  const bot = workspace.bots.find((item) => item.bot_id === params.bot) ?? workspace.bots.find((item) => item.conversation_id === workspace.primary) ?? workspace.bots[0]
  const scope: MessageSearchScope | null = params.task ? { kind: 'task', task_id: params.task } : params.channel ? { kind: 'channel', channel_id: params.channel } : bot ? { kind: 'bot_main', bot_id: bot.bot_id } : null
  const title = scope?.kind === 'task' ? '搜索任务消息' : scope?.kind === 'channel' ? '搜索频道消息' : `搜索 ${bot?.name ?? '聊天'} 的消息`
  const [query, setQuery] = useState('')
  const term = query.trim()
  const scopeKey = JSON.stringify([workspace.networkId, workspace.deviceId, scope])
  const key = JSON.stringify([scopeKey, term, device.ready?.generation])
  const currentKey = useRef(key)
  currentKey.current = key
  const pending = useRef<object | null>(null)
  const [state, setState] = useState<SearchState>({ key: '', items: [], cursor: null, complete: false, kind: 'idle' })
  const local = useMemo(() => {
    if (!workspace.networkId || !workspace.deviceId || scope?.kind !== 'bot_main' || !bot) return []
    const snapshot = localConversations.read({ networkId: workspace.networkId, deviceId: workspace.deviceId, conversationId: bot.conversation_id })
    return (snapshot?.payload.messages ?? []).map((message): Hit => ({ event_hash: message.event_hash, message_id: message.message_id, conversation_id: bot.conversation_id, created_at_ms: message.created_at_ms, text: message.text, rank: message.rank ?? '', source: 'local' }))
  }, [scopeKey])
  const localMatches = term ? local.filter((message) => message.text.toLocaleLowerCase().includes(term.toLocaleLowerCase())) : []
  const visible = state.key === key ? state : { key, items: [], cursor: null, complete: false, kind: 'idle' as const }

  const read = async (cursor?: string) => {
    const connection = device.ready
    if (!scope || !term || !connection?.isCurrent() || pending.current) return
    const owner = {}
    pending.current = owner
    const before = state.key === key && cursor ? state.items : []
    setState({ key, items: before, cursor: cursor ?? null, complete: false, kind: 'loading' })
    try {
      const result = await searchMessages({ connection, scope, query: term, cursor })
      if (currentKey.current !== key || !connection.isCurrent()) return
      const items = new Map(before.map((item) => [item.event_hash, item]))
      for (const item of result.items) items.set(item.event_hash, { ...item, source: 'server' })
      setState({ key, items: [...items.values()], cursor: result.next_cursor, complete: result.complete, kind: 'ready' })
    } catch (cause) {
      if (currentKey.current === key && connection.isCurrent()) setState({ key, items: before, cursor: cursor ?? null, complete: false, kind: 'failed', error: messageOf(cause) })
    } finally { if (pending.current === owner) pending.current = null }
  }
  const latestRead = useRef(read)
  latestRead.current = read
  useEffect(() => {
    pending.current = null
    if (!term || !scope || !device.ready) return
    const timer = setTimeout(() => { void latestRead.current() }, 300)
    return () => clearTimeout(timer)
  }, [key])
  const items = new Map(visible.kind === 'ready' && visible.complete ? [] : localMatches.map((item) => [item.event_hash, item]))
  for (const item of visible.items) items.set(item.event_hash, item)
  const hits = [...items.values()].sort((a, b) => b.rank.localeCompare(a.rank) || b.created_at_ms - a.created_at_ms)

  return <ScrollView keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-12">
    <Stack.Screen options={{ title }} />
    <Input autoFocus accessibilityLabel={title} value={query} onChangeText={setQuery} placeholder="输入消息关键词" maxLength={256} returnKeyType="search" onSubmitEditing={() => void read()} />
    <Text accessibilityLiveRegion="polite" className="text-sm text-muted">{!term ? '先显示本地匹配，联网后检索更早的消息。' : visible.kind === 'loading' ? `已找到 ${hits.length} 条，正在检索完整历史…` : device.ready ? `${hits.length} 条匹配${visible.complete ? ' · 当前范围已检索完毕' : ' · 可继续查找更早结果'}` : `${hits.length} 条本地匹配 · 联网后可检索完整历史`}</Text>
    {hits.map((hit) => {
      const destination = scope?.kind === 'task' ? { pathname: '/task/[taskId]' as const, params: { taskId: scope.task_id, message: hit.message_id } } : scope?.kind === 'channel' ? { pathname: '/channel/[channelId]' as const, params: { channelId: scope.channel_id, message: hit.message_id } } : { pathname: '/b/[botId]' as const, params: { botId: bot?.bot_id ?? '', message: hit.message_id } }
      return <Link key={hit.event_hash} href={destination} asChild><Pressable accessibilityRole="link" accessibilityLabel={`查看原消息，${hit.text}`} className="gap-2 rounded-panel border border-border bg-surface p-4 active:bg-surface-muted">
        <View className="flex-row items-center justify-between gap-2"><Text className="text-sm text-muted">{new Date(hit.created_at_ms).toLocaleString()} · {hit.source === 'local' ? '本地副本' : '已同步历史'}</Text><Icon as={ChevronRight} className="text-muted" /></View>
        <Text numberOfLines={5}>{hit.text}</Text>
      </Pressable></Link>
    })}
    {term && !hits.length && (visible.kind === 'ready' || !device.ready) ? <EmptyState icon={Search} title={device.ready ? '当前已检索的记录没有匹配消息' : '本地没有匹配消息，联网后可继续检索'} /> : null}
    {visible.kind === 'failed' ? <View className="gap-2"><ErrorText error={visible.error} /><Button variant="secondary" onPress={() => void read()}><Text>重新检索</Text></Button></View> : null}
    {device.ready && term && visible.cursor && visible.kind !== 'failed' ? <Button variant="secondary" disabled={visible.kind === 'loading'} onPress={() => void read(visible.cursor ?? undefined)}><Text>{visible.kind === 'loading' ? '正在检索…' : '查找更早的匹配'}</Text></Button> : null}
  </ScrollView>
}
