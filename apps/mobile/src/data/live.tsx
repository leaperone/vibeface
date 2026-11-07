import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { File } from 'expo-file-system'
import { botReach, composerHint } from '@vibeface/client/chat/protocol.ts'
import { byRank, chatTimeline, firstUnread, unreadCounts } from '@vibeface/client/chat/timeline.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { DeviceProvider, useDevice } from '@vibeface/client/identity/context.tsx'
import { entryAddress, messageOf } from '@vibeface/client/lib/node.ts'
import { WorkspaceProvider as SharedWorkspace, useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { botCopy } from '@vibeface/client/workspace/bot-settings.ts'
import { inboxItems, type InboxItem } from '@vibeface/client/workspace/inbox.ts'
import { addressName, deviceOnline, entryLabel } from '@vibeface/client/workspace/labels.ts'
import { botReadiness, parseBots, parsePrimary } from '@vibeface/client/workspace/model.ts'
import { storageOf } from '@vibeface/client/workspace/storage.ts'
import { parseLocalTranscripts, upsertLocalTranscript, type LocalTranscript } from '@vibeface/client/workspace/local-records.ts'
import { isSessionNotification, type BotRecord } from '@vibeface/client/workspace/types.ts'
import { awaitsAnswer, foldAnswers } from '@vibeface/client/workspace/questions.ts'
import { mobilePlatform } from '@/platform'
import { WorkspaceProvider } from './context'
import type { ChatMessage, Connection, Conversation, Member, Session, Workspace, WorkspaceActions } from './types'

const busyStates = ['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling']
const streamingStates = [...busyStates, 'publishing']
const empty = { ownerScope: null, cacheNotice: null, bots: [], primary: null, unread: {}, questions: [], members: [], conversations: {}, chatFocus: null }
const noTabs = { leader: () => true, post() {}, listen: () => () => {} }

/**
 * What this phone last saw of its network, so a cold start that cannot reach it still lists its bots and devices.
 * `me`: this phone's device ID then; leaving or a reset gives the phone a new one, which retires the record, and a removal
 * or a join keeping the ID erases it, so another network never shows this one's.
 */
type LastSeen = { me: string; networkId: string; entry: string; bots: BotRecord[]; primary: string | null; unread: Record<string, number>; members: Member[] }

const { readStorage, writeStorage } = storageOf(mobilePlatform.storage)

function lastSeen(me: string | undefined, networkId?: string): LastSeen | null {
  const value = readStorage<Record<string, unknown> | null>('last-seen', null)
  if (!me || !value || value.me !== me || !Array.isArray(value.members) || networkId && value.networkId !== networkId) return null
  try {
    const members = value.members.filter((member): member is Member => typeof member?.device_id === 'string' && typeof member.name === 'string' && (member.kind === 'node' || member.kind === 'client'))
      .map((member) => ({ ...member, current: member.device_id === me, online: undefined }))
    const unread = value.unread && typeof value.unread === 'object' ? Object.fromEntries(Object.entries(value.unread).filter(([, count]) => typeof count === 'number' && Number.isFinite(count) && count > 0)) : {}
    return { me, networkId: typeof value.networkId === 'string' ? value.networkId : '', entry: typeof value.entry === 'string' ? value.entry : '', bots: parseBots(value), primary: parsePrimary(value), unread, members }
  } catch { return null }
}

const transcriptKey = (networkId: string, deviceId: string) => `local-transcripts.${networkId}.${deviceId}`
function transcriptRecords(networkId: string, deviceId: string): LocalTranscript[] {
  try { return parseLocalTranscripts(mobilePlatform.storage.getItem(`vibeface.${transcriptKey(networkId, deviceId)}`), networkId, deviceId) } catch { return [] }
}
function saveTranscript(record: LocalTranscript, previous: readonly LocalTranscript[]): 'saved' | 'unchanged' | 'failed' {
  const all = upsertLocalTranscript(previous, record)
  if (JSON.stringify(previous) === JSON.stringify(all)) return 'unchanged'
  try { mobilePlatform.storage.setItem(`vibeface.${transcriptKey(record.networkId, record.deviceId)}`, JSON.stringify(all)); return 'saved' } catch { return 'failed' }
}
function clearTranscripts(networkId: string, deviceId: string) {
  try { mobilePlatform.storage.setItem(`vibeface.${transcriptKey(networkId, deviceId)}`, 'null') } catch { /* Cache reads remain disabled for the retired owner. */ }
}

type Shared = ReturnType<typeof useShared>
type ChatVisit = { id: string; workspace: Shared; started: boolean; failed: boolean }
const SharedContext = createContext<Shared | null>(null)

/** The shared workspace while a member connection is up, for screens that call its actions directly; null before that. */
export const useLive = () => useContext(SharedContext)

/** 手机可处理的待处理项；会话提醒由网页 Bell 承载；没有连接时不返回列表。 */
export function useInbox(): InboxItem[] {
  const device = useDevice()
  const shared = useLive()
  return shared ? inboxItems({ device, workspace: shared }).filter((item) => item.kind !== 'session') : []
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
  const device = useDevice()
  const { ready } = device
  if (!ready) return <Offline>{children}</Offline>
  return <SharedWorkspace key={`${ready.principal.network_id}:${ready.principal.device_id}`} platform={mobilePlatform} connection={ready} tabs={noTabs} openConversation={() => {}}>
    <Live>{children}</Live>
  </SharedWorkspace>
}

/** Before a member connection: joining, or resuming saved credentials. */
function Offline({ children }: { children: ReactNode }) {
  const device = useDevice()
  const { session, connection } = useConnection()
  const me = device.identity?.deviceId
  const networkId = device.directory?.network_id
  const stored = useMemo(() => lastSeen(me, networkId), [me, networkId])
  const seen = session.phase === 'ready' && device.phase !== 'removed' && !device.joining ? stored : null
  const gone = device.phase === 'removed' || session.phase === 'joining'
  const owner = seen?.networkId && me ? `${seen.networkId}.${me}:` : null
  const [pending, setPending] = useState<{ owner: string; messages: { conversationId: string; messageId: string; text: string }[]; notice: string | null } | null>(null)
  useEffect(() => {
    if (!owner) return
    let active = true
    const read = () => {
      void mobilePlatform.outbox.all().then((items) => {
        if (!active) return
        const messages = items.flatMap((item) => {
          const { conversation_id, message_id, text } = item.params
          return item.key.startsWith(owner) && item.method === 'message' && item.rejected === undefined
            && typeof conversation_id === 'string' && typeof message_id === 'string' && typeof text === 'string'
            ? [{ conversationId: conversation_id, messageId: message_id, text }] : []
        })
        setPending({ owner, messages, notice: null })
      }).catch(() => { if (active) setPending({ owner, messages: [], notice: '无法读取待确认的消息，请稍后重试。' }) })
    }
    read()
    const stop = mobilePlatform.onForeground(read)
    return () => { active = false; stop() }
  }, [owner])
  useEffect(() => { if (gone) { writeStorage('last-seen', null); if (stored?.networkId && me) clearTranscripts(stored.networkId, me) } }, [gone, stored?.networkId, me])
  const actions = useMemo<WorkspaceActions>(() => ({
    join: (invitation, name) => device.join(invitation, name),
    cancelJoin: device.cancelJoin,
    open: () => () => {},
    send: async () => { throw new Error('还没连上') },
    stop: async () => {},
    answer: async () => { throw new Error('还没连上') },
    leave: device.leave,
  }), [device])
  // The entry it last reached is the one that does not answer now, so it alone shows as offline.
  const kept = seen ? { bots: seen.bots, primary: seen.primary, members: seen.members.map((member) => member.entry ? { ...member, online: false } : member) } : {}
  const conversations: Record<string, Conversation> = {}
  const records = useMemo(() => seen?.networkId && me ? transcriptRecords(seen.networkId, me) : [], [seen?.networkId, me])
  const byConversation = new Map(records.map((record) => [record.conversationId, record]))
  if (seen?.networkId && me) for (const bot of seen.bots) {
    const cached = byConversation.get(bot.conversation_id)
    if (cached?.messages.length) conversations[bot.conversation_id] = cachedConversation(cached)
  }
  if (seen?.networkId && me && pending?.owner === owner) for (const message of pending.messages) {
    const chat = conversations[message.conversationId] ?? cachedConversation({ networkId: seen.networkId, deviceId: me, conversationId: message.conversationId, messages: [] })
    if (!chat.messages.some((item) => item.message_id === message.messageId)) chat.messages.push({ message_id: message.messageId, event_hash: message.messageId,
      role: 'user', text: message.text, created_at_ms: Date.now(), state: 'sending' })
    conversations[message.conversationId] = chat
  }
  return <WorkspaceProvider value={{ ...empty, ...kept, ownerScope: seen?.networkId && me ? { networkId: seen.networkId, deviceId: me } : null,
    cacheNotice: pending?.owner === owner ? pending.notice : null,
    unread: seen?.unread ?? {}, conversations, session, connection: { ...connection, entry: connection.entry || seen?.entry || '' }, actions }}>{children}</WorkspaceProvider>
}

function cachedConversation(record: LocalTranscript): Conversation {
  return { source: 'cache', messages: [...record.messages], before: new Map(), rest: [], answers: new Map(), hidden: new Set(), unread: null, works: [], replying: false,
    status: '显示本机记录，正在更新', stoppable: false, blocked: '本机记录只读，正在更新', sending: false }
}

function Live({ children }: { children: ReactNode }) {
  const device = useDevice()
  const shared = useShared()
  const { session, connection } = useConnection()
  const latest = useRef(shared)
  latest.current = shared
  const me = device.identity?.deviceId
  const networkId = device.ready?.principal.network_id
  const cacheOwnerActive = device.phase !== 'removed' && !device.joining
  const { data, conversationId, nodes, questions, notifications, answered } = shared
  // Until the first read lands the list keeps what the phone last saw instead of going blank.
  const seen = useMemo(() => cacheOwnerActive ? lastSeen(me, networkId) : null, [me, networkId, cacheOwnerActive])
  const [cacheRevision, setCacheRevision] = useState(0)
  const [cacheNotice, setCacheNotice] = useState<string | null>(null)
  const records = useMemo(() => cacheOwnerActive && networkId && me ? transcriptRecords(networkId, me) : [], [cacheOwnerActive, networkId, me, cacheRevision])
  useEffect(() => { if (!cacheOwnerActive && networkId && me) clearTranscripts(networkId, me) }, [cacheOwnerActive, networkId, me])
  const byConversation = useMemo(() => new Map(records.map((record) => [record.conversationId, record])), [records])

  const [chatFocus, setChatFocus] = useState<Workspace['chatFocus']>(null)
  const focusRef = useRef(chatFocus)
  focusRef.current = chatFocus

  const conversations: Record<string, Conversation> = {}
  if (cacheOwnerActive && data && conversationId && data.conversation?.conversation_id === conversationId) {
    const cached = byConversation.get(conversationId)?.messages ?? []
    const missing = cached.filter((message) => !data.messages.some((item) => item.message_id === message.message_id))
    const history = missing.length ? byRank([...data.messages, ...missing]) : data.messages
    const jobs = data.scheduler?.jobs ?? []
    // A streamed reply gives way once its job ends, or as soon as the history's newest message is the reply it was
    // streaming, which can land a frame or two before the job names it.
    const newest = data.messages.at(-1)
    // Either side may be ahead by the last few characters, so one being the start of the other is the same reply.
    // A message still on its way means the newest in the history is the last turn's reply, not this one's.
    const sending = shared.messagePending && !data.messages.some((message) => message.message_id === shared.messagePending?.message_id)
    const landed = (text: string) => !sending && newest?.role === 'assistant' && newest.text.trim() !== '' && text.trim() !== ''
      && (newest.text.trim().startsWith(text.trim()) || text.trim().startsWith(newest.text.trim()))
    const streamed = shared.liveProgress.filter((progress): progress is Extract<typeof progress, { kind: 'manager' }> => progress.kind === 'manager' && progress.conversation_id === conversationId && Boolean(progress.text))
    const handedOver = streamed.some((progress) => landed(progress.text))
    const live = streamed.find((progress) => !landed(progress.text)
      && !jobs.some((job) => job.trigger === progress.trigger && (!streamingStates.includes(job.state) || job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))))
    // The user message it answers, as the final reply's in_reply_to names it, so the two share a row: the one still
    // sending, or else the latest in the history.
    const liveTrigger = live && (sending ? shared.messagePending?.message_id : data.messages.findLast((message) => message.role === 'user')?.message_id)
    const timeline = chatTimeline(history, data.questions, new Set(jobs.filter((job) => job.state === 'steered').map((job) => job.message_id)))
    // An answer is a user message, shown inside its question instead of as a bubble, as on the web.
    const folded = foldAnswers(data.messages, data.questions)
    const unread = firstUnread(timeline, shared.fresh, (question) => awaitsAnswer(question, answered, folded.answers) && shared.fresh.has(`question:${question.question_id}`))
    const messages: ChatMessage[] = timeline.messages.map((message) => {
      const detail = message.status === 'failed' ? jobs.find((job) => job.reply_message_id === message.message_id)?.failure?.detail : undefined
      return detail ? { ...message, detail } : message
    })
    for (const reply of shared.interrupted) messages.push({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: Date.now(), state: 'interrupted' })
    const pending = shared.messagePending
    if (pending && !data.messages.some((message) => message.message_id === pending.message_id)) messages.push({ message_id: pending.message_id, event_hash: pending.message_id, role: 'user', text: pending.text, created_at_ms: Date.now(), state: 'sending' })
    if (live) messages.push({ message_id: `live:${live.key}`, event_hash: `live:${live.key}`, role: 'assistant', text: live.text, created_at_ms: Date.now(), state: 'streaming', in_reply_to: liveTrigger })
    const stopJob = jobs.find((job) => data.scheduler?.capabilities?.cancel === true && ['queued', 'steering', 'claimed', 'running'].includes(job.state))
    conversations[conversationId] = {
      source: missing.length ? 'cache' : 'live', messages, before: timeline.before, rest: timeline.rest, answers: folded.answers, hidden: folded.hidden, works: data.works,
      unread: unread ? 'question_id' in unread ? `q:${unread.question_id}` : unread.message_id : null,
      // A finished job whose reply has not synced yet still counts, so the activity row hands over to the reply instead of
      // vanishing a frame early; its trigger being shown, even still sending, means the newer reply is on its way, not paged out.
      replying: Boolean(live) || !handedOver && jobs.some((job) => busyStates.includes(job.state) && !(job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))
        || job.reply_message_id && job.message_id
        && !data.messages.some((message) => message.message_id === job.reply_message_id) && messages.some((message) => message.message_id === job.message_id)),
      status: jobs.some((job) => job.state === 'cancelling') ? '正在停止…' : shared.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status ?? null,
      stoppable: Boolean(stopJob && shared.writable),
      blocked: shared.readOnlyNotice || (missing.length ? '正在补读记录，稍后可发送' : '') || (shared.writeNotice ? '同步中，稍后可发送' : '') || composerHint({ hasConversation: true, loaded: true, bot: botReach(nodes) }) || null,
      sending: shared.messageBusy,
    }
  }

  if (cacheOwnerActive && networkId && me) {
    const id = conversationId ?? ''
    const cached = id ? byConversation.get(id) ?? null : null
    if (id && !conversations[id] && cached?.messages.length) conversations[id] = cachedConversation(cached)
  }
  const transcriptMessages = cacheOwnerActive && networkId && me && conversationId && data?.conversation?.conversation_id === conversationId && !shared.reading && !shared.error
    ? data.messages : null
  useEffect(() => {
    if (!cacheOwnerActive || !networkId || !me || !conversationId || !transcriptMessages) return
    const result = saveTranscript({ networkId, deviceId: me, conversationId, messages: transcriptMessages }, records)
    if (result === 'saved') { setCacheNotice(null); setCacheRevision((revision) => revision + 1) }
    if (result === 'unchanged') setCacheNotice(null)
    if (result === 'failed') setCacheNotice('无法保存本机聊天记录。')
  }, [cacheOwnerActive, networkId, me, conversationId, transcriptMessages, records])
  if (cacheOwnerActive && conversationId && shared.messagePending?.conversation_id === conversationId) {
    const current = conversations[conversationId] ?? cachedConversation({ networkId: networkId ?? '', deviceId: me ?? '', conversationId, messages: [] })
    if (!current.messages.some((message) => message.message_id === shared.messagePending?.message_id)) {
      conversations[conversationId] = { ...current, messages: [...current.messages, { message_id: shared.messagePending.message_id,
        event_hash: shared.messagePending.message_id, role: 'user', text: shared.messagePending.text, created_at_ms: Date.now(), state: 'sending' }] }
    }
  }

  const currentUnread = unreadCounts(notifications.filter((item) => !isSessionNotification(item) && !shared.ignoredNotifications.has(item.notification_id)), questions.filter((question) => awaitsAnswer(question, answered)))
  const unread = cacheOwnerActive ? shared.synced ? currentUnread : seen?.unread ?? currentUnread : {}

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
    await workspace.selectBot(bot)
  }
  const focused = useRef<ChatVisit | null>(null)
  const currentVisit = (visit: ChatVisit) => focused.current === visit && visit.workspace.isCurrent() && latest.current.generation === visit.workspace.generation
  const activate = async (visit: ChatVisit) => {
    if (!currentVisit(visit) || visit.started || visit.failed || !latest.current.bots.some((bot) => bot.conversation_id === visit.id)) return
    visit.started = true
    setChatFocus({ kind: 'loading', id: visit.id })
    try {
      await select(visit.id)
      if (currentVisit(visit)) {
        latest.current.setView('chat')
        setChatFocus({ kind: 'ready', id: visit.id })
      }
    } catch (cause) {
      visit.started = false
      visit.failed = true
      if (currentVisit(visit)) {
        const retry = () => { visit.started = false; visit.failed = false; void activate(visit).catch(() => {}) }
        setChatFocus({ kind: 'error', id: visit.id, message: messageOf(cause, '读取聊天记录'), retry })
      }
      throw cause
    }
  }
  // A kept route waits for its Bot; it cannot expose the previous selection as a visible chat.
  useEffect(() => {
    if (focused.current) void activate(focused.current).catch(() => {})
  }, [shared.bots, shared.generation])
  const actions = useMemo<WorkspaceActions>(() => {
    return {
      join: (invitation, name) => device.join(invitation, name),
      cancelJoin: device.cancelJoin,
      open(id) {
        const workspace = latest.current
        if (!workspace.isCurrent()) return () => {}
        const visit = { id, workspace, started: false, failed: false }
        focused.current = visit
        setChatFocus(null)
        workspace.setView('other')
        void activate(visit).catch(() => {})
        return () => {
          if (focused.current !== visit) return
          focused.current = null
          setChatFocus(null)
          if (workspace.isCurrent() && latest.current.generation === workspace.generation) latest.current.setView('other')
        }
      },
      async send(id, text, attachments) {
        const current = latest.current
        if (current.conversationId !== id || current.data?.conversation?.conversation_id !== id) await select(id)
        return latest.current.sendMessage(text, attachments.map((file) => ({ name: file.name, arrayBuffer: () => new File(file.uri).arrayBuffer() })))
      },
      async stop(id) {
        await select(id)
        const job = latest.current.data?.scheduler?.jobs.find((item) => ['queued', 'steering', 'claimed', 'running'].includes(item.state))
        if (job) await latest.current.cancelManager(job.trigger)
      },
      answer: (question, text) => latest.current.answer(question, text),
      leave: device.leave,
    }
  }, [device.join, device.cancelJoin, device.leave])

  useEffect(() => {
    const focus = focusRef.current
    if (focus && shared.conversationId === focus.id && shared.data?.conversation?.conversation_id === focus.id && !shared.reading) {
      setChatFocus((current) => current?.id === focus.id && current.kind === 'loading' ? { kind: 'ready', id: focus.id } : current)
    }
  }, [shared.error, shared.conversationId, shared.data, shared.reading])

  const fresh = shared.synced || shared.bots.length > 0
  const bots = !cacheOwnerActive ? [] : fresh || !seen ? shared.bots : seen.bots, primary = !cacheOwnerActive ? null : fresh || !seen ? shared.primary : seen.primary
  const members = device.directory || !seen ? directory : seen.members
  const snapshot = cacheOwnerActive && me && networkId && shared.synced && device.directory ? { me, networkId, entry: connection.entry, bots: shared.bots.map((bot) => ({ ...bot, busy: null })), primary: shared.primary, unread, members: directory } : null
  const record = snapshot && JSON.stringify(snapshot)
  useEffect(() => { if (snapshot) writeStorage('last-seen', snapshot) }, [record])

  const value: Workspace = {
    ownerScope: cacheOwnerActive && networkId && me ? { networkId, deviceId: me } : cacheOwnerActive && seen?.networkId && me && session.phase === 'ready' ? { networkId: seen.networkId, deviceId: me } : null,
    cacheNotice,
    session, connection: { ...connection, entry: connection.entry || seen?.entry || '' }, bots, primary, unread, questions, members, conversations, chatFocus, actions,
  }
  return <SharedContext.Provider value={shared}><WorkspaceProvider value={value}>{children}</WorkspaceProvider></SharedContext.Provider>
}
