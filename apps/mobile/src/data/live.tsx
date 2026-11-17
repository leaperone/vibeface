import type { DisplayScope } from '@vibeface/client/identity/context.tsx'
import { decodeLastSeen, displayLastSeen, projectLastSeen } from './last-seen'
import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import * as Notifications from 'expo-notifications'
import { chatTimeline, firstUnread, unreadCounts } from '@vibeface/client/chat/timeline.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { DeviceProvider, useDevice } from '@vibeface/client/identity/context.tsx'
import { entryAddress, messageOf } from '@vibeface/client/lib/node.ts'
import { WorkspaceProvider as SharedWorkspace, useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { botCopy } from '@vibeface/client/workspace/bot-settings.ts'
import { inboxItems, waitingItems, type InboxItem } from '@vibeface/client/workspace/inbox.ts'
import { addressName, deviceOnline, entryLabel } from '@vibeface/client/workspace/labels.ts'
import { botReadiness, parsePendingMessage } from '@vibeface/client/workspace/model.ts'
import type { PendingMessage } from '@vibeface/client/chat/types.ts'
import { notificationBadgeCount, notificationPreferences, notificationUnreadCounts, watchNotificationPreferences } from '@vibeface/client/workspace/system-notifications.ts'
import { storageOf } from '@vibeface/client/workspace/storage.ts'
import { isSessionNotification } from '@vibeface/client/workspace/types.ts'
import { awaitsAnswer, foldAnswers } from '@vibeface/client/workspace/questions.ts'
import { mobilePlatform } from '@/platform'
import { remoteNotifications } from '@/platform/remote-notifications'
import { cachedConversation, localConversations } from './cached'
import { WorkspaceProvider } from './context'
import type { ChatMessage, Connection, Conversation, Member, Session, Workspace, WorkspaceActions } from './types'
import type { InputTarget } from '@vibeface/client/workspace/types.ts'
import { useRouter } from 'expo-router'

const busyStates = ['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling']
const streamingStates = [...busyStates, 'publishing']
const empty = { networkId: null, deviceId: null, bots: [], primary: null, unread: {}, questions: [], members: [], conversations: {} }
const noTabs = { leader: () => true, post() {}, listen: () => () => {} }

const { readStorage, writeStorage } = storageOf(mobilePlatform.storage)
function lastSeen(scope: DisplayScope | null) {
  const cached = decodeLastSeen(readStorage('last-seen', null), scope)
  return cached && displayLastSeen(cached)
}

type Shared = ReturnType<typeof useShared>
type ChatVisit = { id: string; workspace: Shared; started: boolean }
const SharedContext = createContext<Shared | null>(null)

/** The shared workspace while a member connection is up, for screens that call its actions directly; null before that. */
export const useLive = () => useContext(SharedContext)

/** 手机可处理的待处理项；会话提醒由网页 Bell 承载；没有连接时不返回列表。 */
export function useInbox(): InboxItem[] {
  const device = useDevice()
  const shared = useLive()
  return shared ? inboxItems({ device, workspace: shared }).filter((item) => item.kind !== 'session') : []
}

/** How many things wait on the person, as the home card and the 待处理 tab count them. */
export function useWaiting(): number {
  const shared = useLive()
  return waitingItems(useInbox()).length + (shared?.workspaceState?.tasks ?? []).filter((task) => ['waiting_user', 'needs_reconciliation', 'update_pending', 'stop_pending'].includes(task.state)).length
}

/** The real network behind the screens: the shared device and workspace providers, reshaped for `useWorkspace`. */
export function LiveWorkspace({ children }: { children: ReactNode }) {
  // A factory reset remounts everything below, back to the welcome page.
  const [run, setRun] = useState(0)
  return <DeviceProvider key={run} platform={mobilePlatform} restart={() => setRun((count) => count + 1)}><Device>{children}</Device></DeviceProvider>
}

function useConnection(): { session: Session; connection: Connection } {
  const device = useDevice()
  const access = accessOf({ ready: Boolean(device.ready), phase: device.phase, joining: Boolean(device.joining), savedEntries: device.entries.length, invitation: false, offlineRetryMs: null })
  // A recovery still waiting, or one whose new file is not confirmed as saved yet, holds the phone on the join screen.
  const session: Session = device.recovery || device.ready && device.recovered ? { phase: 'unjoined' }
    : access.kind === 'member' || access.kind === 'resuming' || access.kind === 'loading' && device.entries.length ? { phase: 'ready' }
    : device.joining ? { phase: 'joining', code: device.joining.code ?? null, admin: device.joining.admin }
    : { phase: 'unjoined' }
  const { link } = device
  const entry = device.ready?.entry ?? device.entry
  const name = device.ready ? entryLabel({ joined: device.ready.principal.source === 'membership', directory: device.directory, entryId: device.ready.entry.device_id, ownName: device.ready.info.name, address: entryAddress(device.ready.entry) })
    : entry ? device.entryName(entry) || addressName(entryAddress(entry)) : ''
  const state: Connection['state'] = name === null ? 'connecting' : link.kind === 'online' ? (link.catchingUp ? 'syncing' : 'online') : link.kind === 'offline' ? 'offline' : 'connecting'
  return { session, connection: { state, entry: name ?? '', direct: entry?.kind === 'lan' } }
}

function Device({ children }: { children: ReactNode }) {
  const router = useRouter()
  const device = useDevice()
  const { ready } = device
  if (!ready) return <Offline>{children}</Offline>
  return <SharedWorkspace key={`${ready.principal.network_id}:${ready.principal.device_id}`} platform={mobilePlatform} connection={ready} tabs={noTabs} openConversation={(id, target?: InputTarget) => {
    if (target?.kind === 'task' || target?.kind === 'channel_task') router.push(`/task/${target.task_id}`)
    else if (target?.kind === 'channel_member') router.push(`/channel/${target.channel_id}`)
    else router.push({ pathname: '/', params: { conversation: id, ...(target?.kind === 'work' ? { work: target.work_id } : target?.kind === 'routine' ? { routine: target.trigger } : {}) } })
  }}>
    <Live>{children}</Live>
  </SharedWorkspace>
}

/** Before a member connection: joining, or resuming saved credentials. */
function Offline({ children }: { children: ReactNode }) {
  const device = useDevice()
  const { session, connection } = useConnection()
  const scope = device.displayScope
  const seen = useMemo(() => session.phase === 'ready' ? lastSeen(scope) : null, [session.phase, scope?.network_id, scope?.device_id, scope?.entry_device_id])
  const scopeKey = JSON.stringify(scope)
  const [pendingState, setPendingState] = useState<{ key: string; items: PendingMessage[]; error: string }>({ key: '', items: [], error: '' })
  const pending = pendingState.key === scopeKey ? pendingState.items : []
  const pendingError = pendingState.key === scopeKey ? pendingState.error : ''
  useEffect(() => {
    let active = true
    if (!seen || !scope) { setPendingState({ key: scopeKey, items: [], error: '' }); return }
    const prefix = `${scope.network_id}.${scope.device_id}:`
    void mobilePlatform.outbox.all().then((items) => {
      const messages = items.filter((item) => item.key.startsWith(prefix) && item.method === 'message').map(parsePendingMessage)
      if (active) setPendingState({ key: scopeKey, items: messages, error: '' })
    }).catch((cause) => { if (active) setPendingState({ key: scopeKey, items: [], error: messageOf(cause, '读取本地待发消息') }) })
    return () => { active = false }
  }, [seen, scopeKey])
  const conversations = useMemo(() => Object.fromEntries(seen && scope ? localConversations.all({ networkId: scope.network_id, deviceId: scope.device_id }).map((record) => {
    const conversation = cachedConversation(record, pending)
    return [record.conversationId, pendingError ? { ...conversation, blocked: pendingError } : conversation]
  }) : []), [seen, scopeKey, pending, pendingError])
  const gone = device.phase === 'removed' || session.phase === 'joining'
  const generation = device.connectionGeneration()
  useEffect(() => { if (gone && device.connectionGeneration() === generation) writeStorage('last-seen', null) }, [gone, generation])
  const actions = useMemo<WorkspaceActions>(() => ({
    join: (invitation, name) => device.join(invitation, name),
    cancelJoin: device.cancelJoin,
    open: () => () => {},
    send: async () => { throw new Error('还没连上') },
    answer: async () => { throw new Error('还没连上') },
    leave: device.leave,
  }), [device])
  // The entry it last reached is the one that does not answer now, so it alone shows as offline.
  const kept = seen ? { bots: seen.bots, primary: seen.primary, members: seen.members.map((member) => member.entry ? { ...member, online: false } : member) } : {}
  return <WorkspaceProvider value={{ ...empty, ...kept, networkId: scope?.network_id ?? null, deviceId: scope?.device_id ?? null, conversations, session, connection: { ...connection, entry: connection.entry || seen?.entry || '' }, actions }}>{children}</WorkspaceProvider>
}

function Live({ children }: { children: ReactNode }) {
  const device = useDevice()
  const shared = useShared()
  const { session, connection } = useConnection()
  const latest = useRef(shared)
  latest.current = shared
  const me = device.identity?.deviceId
  const networkId = device.ready?.principal.network_id ?? ''
  const { data, conversationId, nodes, questions, notifications, answered } = shared
  // Until the first read lands the list keeps what the phone last saw instead of going blank.
  const scope = device.displayScope
  const seen = useMemo(() => lastSeen(scope), [scope?.network_id, scope?.device_id, scope?.entry_device_id, device.phase])

  const cached = useMemo(() => localConversations.all({ networkId, deviceId: me ?? '' }), [networkId, me, data, shared.generation])
  const conversations: Record<string, Conversation> = Object.fromEntries(cached.map((record) => [record.conversationId, cachedConversation(record, shared.pendingMessages)]))
  if (data && conversationId && shared.dataOrigin === 'cache') conversations[conversationId] = cachedConversation({ networkId, deviceId: me ?? '', conversationId, savedAt: cached.find((record) => record.conversationId === conversationId)?.savedAt ?? Date.now(), payload: data }, shared.pendingMessages)
  if (data && conversationId && shared.dataOrigin === 'network') {
    const jobs = data.scheduler?.jobs ?? []
    // A streamed reply gives way once its job ends, or as soon as the history's newest message is the reply it was
    // streaming, which can land a frame or two before the job names it.
    const newest = data.messages.at(-1)
    // Either side may be ahead by the last few characters, so one being the start of the other is the same reply.
    // A message still on its way means the newest in the history is the last turn's reply, not this one's.
    const pending = shared.pendingMessages.filter((item) => item.conversation_id === conversationId && item.target.kind === 'bot_main')
    const sending = pending.some((item) => !data.messages.some((message) => message.message_id === item.message_id))
    const landed = (text: string) => !sending && newest?.role === 'assistant' && newest.text.trim() !== '' && text.trim() !== ''
      && (newest.text.trim().startsWith(text.trim()) || text.trim().startsWith(newest.text.trim()))
    const streamed = shared.liveProgress.filter((progress): progress is Extract<typeof progress, { kind: 'manager' }> => progress.kind === 'manager' && progress.conversation_id === conversationId && Boolean(progress.text))
    const handedOver = streamed.some((progress) => landed(progress.text))
    const live = streamed.find((progress) => !landed(progress.text)
      && !jobs.some((job) => job.trigger === progress.trigger && (!streamingStates.includes(job.state) || job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))))
    const liveTrigger = live && (jobs.find((job) => job.trigger === live.trigger)?.message_id ?? (sending ? pending.at(-1)?.message_id : data.messages.findLast((message) => message.role === 'user')?.message_id))
    const timeline = chatTimeline(data.messages, data.questions, new Set(jobs.filter((job) => job.state === 'steered').map((job) => job.message_id)))
    // An answer is a user message, shown inside its question instead of as a bubble, as on the web.
    const folded = foldAnswers(data.messages, data.questions)
    const unread = firstUnread(timeline, shared.fresh, (question) => awaitsAnswer(question, answered, folded.answers) && shared.fresh.has(`question:${question.question_id}`))
    const messages: ChatMessage[] = timeline.messages.map((message) => {
      const detail = message.status === 'failed' ? jobs.find((job) => job.reply_message_id === message.message_id)?.failure?.detail : undefined
      return detail ? { ...message, detail } : message
    })
    for (const reply of shared.interrupted) messages.push({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: Date.now(), state: 'interrupted' })
    for (const item of pending) if (!data.messages.some((message) => message.message_id === item.message_id)) messages.push({ message_id: item.message_id, event_hash: item.message_id, role: 'user', text: item.text, created_at_ms: item.created_at_ms, state: 'sending' })
    if (live) messages.push({ message_id: `live:${live.key}`, event_hash: `live:${live.key}`, role: 'assistant', text: live.text, created_at_ms: Date.now(), state: 'streaming', in_reply_to: liveTrigger })
    const stopJob = jobs.find((job) => data.scheduler?.capabilities?.cancel === true && ['queued', 'steering', 'claimed', 'running'].includes(job.state))
    conversations[conversationId] = {
      origin: { kind: 'network' },
      messages, before: timeline.before, rest: timeline.rest, answers: folded.answers, hidden: folded.hidden, works: data.works,
      unread: unread ? 'question_id' in unread ? `q:${unread.question_id}` : unread.message_id : null,
      // A finished job whose reply has not synced yet still counts, so the activity row hands over to the reply instead of
      // vanishing a frame early; its trigger being shown, even still sending, means the newer reply is on its way, not paged out.
      replying: Boolean(live) || !handedOver && jobs.some((job) => busyStates.includes(job.state) && !(job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))
        || ['publishing', 'completed', 'failed', 'interrupted'].includes(job.state) && job.reply_message_id && job.message_id
        && !data.messages.some((message) => message.message_id === job.reply_message_id) && messages.some((message) => message.message_id === job.message_id)),
      status: jobs.some((job) => job.state === 'cancelling') ? '正在停止…' : shared.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status ?? null,
      stoppable: Boolean(stopJob && shared.writable),
      blocked: shared.canSaveMessages ? null : shared.readOnlyNotice || shared.writeNotice || '尚未完成首读',
    }
  }

  const unread = unreadCounts(notifications.filter((item) => { const target = item.target; return target?.kind === 'bot_main' && shared.bots.some((bot) => bot.bot_id === target.bot_id && bot.conversation_id === item.conversation_id) && !isSessionNotification(item) && !shared.ignoredNotifications.has(item.notification_id) }), questions.filter((question) => question.target?.kind === 'bot_main' && shared.bots.some((bot) => question.target?.kind === 'bot_main' && bot.bot_id === question.target.bot_id && bot.conversation_id === question.conversation_id) && awaitsAnswer(question, answered)))

  const directory: Member[] = (device.directory?.members ?? []).map((member) => {
    const node = nodes.find((item) => item.device_id === member.device_id), entry = member.device_id === device.ready?.entry.device_id
    return {
      device_id: member.device_id, name: member.name, kind: member.kind === 'node' ? 'node' : 'client', admin: member.manage,
      online: deviceOnline(member.kind, node, entry, shared.connectionOnline, member.device_id === me),
      current: member.device_id === me, entry, bot: botReadiness(node),
    }
  })

  const select = async (id: string) => {
    const workspace = latest.current
    const bot = workspace.bots.find((item) => item.conversation_id === id)
    if (!bot) throw new Error(botCopy.notFound)
    if (workspace.conversationId !== id) await workspace.selectBot(bot)
  }
  const focused = useRef<ChatVisit | null>(null)
  const currentVisit = (visit: ChatVisit) => focused.current === visit && visit.workspace.isCurrent() && latest.current.generation === visit.workspace.generation
  const activate = async (visit: ChatVisit) => {
    if (!currentVisit(visit) || visit.started || !latest.current.bots.some((bot) => bot.conversation_id === visit.id)) return
    visit.started = true
    try {
      await select(visit.id)
      if (currentVisit(visit)) latest.current.setView('chat')
    } catch { visit.started = false }
  }
  // A kept route waits for its Bot; it cannot expose the previous selection as a visible chat.
  useEffect(() => {
    if (focused.current) void activate(focused.current)
  }, [shared.bots, shared.generation])
  const actions = useMemo<WorkspaceActions>(() => {
    return {
      join: (invitation, name) => device.join(invitation, name),
      cancelJoin: device.cancelJoin,
      open(id) {
        const workspace = latest.current
        if (!workspace.isCurrent()) return () => {}
        const visit = { id, workspace, started: false }
        focused.current = visit
        workspace.setView('other')
        void activate(visit)
        return () => {
          if (focused.current !== visit) return
          focused.current = null
          if (workspace.isCurrent() && latest.current.generation === workspace.generation) latest.current.setView('other')
        }
      },
      async send(id, text, attachments, options) {
        const workspace = latest.current
        const bot = workspace.bots.find((item) => item.conversation_id === id)
        if (!bot || !workspace.isCurrent()) throw new Error('原对话尚未读取，请重新连接后发送。')
        await workspace.sendMessage(text, attachments.map((file) => ({ name: file.name, arrayBuffer: file.arrayBuffer })), undefined, { ...options, conversation_id: id, target: options?.target ?? { kind: 'bot_main', bot_id: bot.bot_id } })
      },
      answer: (question, text) => latest.current.answer(question, text),
      leave: device.leave,
    }
  }, [device.join, device.cancelJoin, device.leave])

  useEffect(() => { if (shared.error) console.warn(shared.error) }, [shared.error])

  const fresh = shared.synced || shared.bots.length > 0
  const bots = fresh || !seen ? shared.bots : seen.bots, primary = fresh || !seen ? shared.primary : seen.primary
  const members = device.directory || !seen ? directory : seen.members
  const ready = device.ready, generation = ready?.generation
  const snapshot = scope && me === scope.device_id && shared.synced && device.directory && ready?.isCurrent() ? projectLastSeen(scope, connection.entry, shared.bots, shared.primary, directory) : null
  const record = snapshot && JSON.stringify(snapshot)
  useEffect(() => {
    if (snapshot && ready?.isCurrent() && ready.generation === generation && ready.principal.network_id === snapshot.network_id && ready.principal.device_id === snapshot.device_id && ready.entry.device_id === snapshot.entry_device_id) writeStorage('last-seen', snapshot)
  }, [record, ready, generation])
  useEffect(() => {
    const ready = device.ready
    if (!ready) return
    const sync = () => { if (ready.isCurrent()) void remoteNotifications.sync(ready, notificationPreferences(mobilePlatform.storage, ready.principal.network_id).read()) }
    sync()
    const stopForeground = mobilePlatform.onForeground(sync)
    const tokens = Notifications.addPushTokenListener(sync)
    return () => { stopForeground(); tokens.remove() }
  }, [device.ready, shared.generation])
  const badgeUnread = notificationUnreadCounts(notifications)
  const unreadVersion = JSON.stringify(badgeUnread)
  useEffect(() => {
    if (!shared.synced) return
    const update = () => {
      const preferences = notificationPreferences(mobilePlatform.storage, networkId).read()
      void Notifications.setBadgeCountAsync(notificationBadgeCount(badgeUnread, preferences)).catch(() => {})
    }
    update()
    return watchNotificationPreferences(networkId, update)
  }, [shared.synced, networkId, unreadVersion])

  const value: Workspace = {
    networkId, deviceId: me ?? null, session, connection: { ...connection, entry: connection.entry || seen?.entry || '' }, bots, primary, unread, questions, members, conversations, actions,
  }
  return <SharedContext.Provider value={shared}><WorkspaceProvider value={value}>{children}</WorkspaceProvider></SharedContext.Provider>
}
