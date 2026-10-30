import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { File } from 'expo-file-system'
import { botReach, composerHint } from '@vibeface/client/chat/protocol.ts'
import { chatTimeline, firstUnread, unreadCounts } from '@vibeface/client/chat/timeline.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { DeviceProvider, useDevice } from '@vibeface/client/identity/context.tsx'
import { entryAddress } from '@vibeface/client/lib/node.ts'
import { WorkspaceProvider as SharedWorkspace, useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { botCopy } from '@vibeface/client/workspace/bot-settings.ts'
import { inboxItems, type InboxItem } from '@vibeface/client/workspace/inbox.ts'
import { addressName, deviceOnline } from '@vibeface/client/workspace/labels.ts'
import { botReadiness, parseBots, parsePrimary } from '@vibeface/client/workspace/model.ts'
import { storageOf } from '@vibeface/client/workspace/storage.ts'
import type { BotRecord } from '@vibeface/client/workspace/types.ts'
import { awaitsAnswer, foldAnswers } from '@vibeface/client/workspace/questions.ts'
import { mobilePlatform } from '@/platform'
import { WorkspaceProvider } from './context'
import type { ChatMessage, Connection, Conversation, Member, Session, Workspace, WorkspaceActions } from './types'

const busyStates = ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling']
const empty = { bots: [], primary: null, unread: {}, questions: [], members: [], conversations: {} }
const noTabs = { leader: () => true, post() {}, listen: () => () => {} }

/**
 * What this phone last saw of its network, so a cold start that cannot reach it still lists its bots and devices.
 * `me`: this phone's device ID then; leaving or a reset gives the phone a new one, which retires the record, and a removal
 * or a join keeping the ID erases it, so another network never shows this one's.
 */
type LastSeen = { me: string; entry: string; bots: BotRecord[]; primary: string | null; members: Member[] }

const { readStorage, writeStorage } = storageOf(mobilePlatform.storage)

function lastSeen(me: string | undefined): LastSeen | null {
  const value = readStorage<Record<string, unknown> | null>('last-seen', null)
  if (!me || !value || value.me !== me || !Array.isArray(value.members)) return null
  try {
    const members = value.members.filter((member): member is Member => typeof member?.device_id === 'string' && typeof member.name === 'string' && (member.kind === 'node' || member.kind === 'client'))
      .map((member) => ({ ...member, current: member.device_id === me, online: undefined }))
    return { me, entry: typeof value.entry === 'string' ? value.entry : '', bots: parseBots(value), primary: parsePrimary(value), members }
  } catch { return null }
}

type Shared = ReturnType<typeof useShared>
const SharedContext = createContext<Shared | null>(null)

/** The shared workspace while a member connection is up, for screens that call its actions directly; null before that. */
export const useLive = () => useContext(SharedContext)

/** 待处理, item for item as the web lists it; nothing before a member connection. */
export function useInbox(): InboxItem[] {
  const device = useDevice()
  const shared = useLive()
  return shared ? inboxItems({ device, workspace: shared }) : []
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
    : device.joining ? { phase: 'joining', code: device.joining.code ?? null }
    : { phase: 'unjoined' }
  const { link } = device
  const state: Connection['state'] = link.kind === 'online' ? (link.catchingUp ? 'syncing' : 'online') : link.kind === 'offline' ? 'offline' : 'connecting'
  const entry = device.ready?.entry ?? device.entry
  return { session, connection: { state, entry: device.ready?.info.name ?? (entry ? device.entryName(entry) || addressName(entryAddress(entry)) : ''), direct: entry?.kind === 'lan' } }
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
  const seen = useMemo(() => session.phase === 'ready' ? lastSeen(me) : null, [session.phase, me])
  const gone = device.phase === 'removed' || session.phase === 'joining'
  useEffect(() => { if (gone) writeStorage('last-seen', null) }, [gone])
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
  return <WorkspaceProvider value={{ ...empty, ...kept, session, connection: { ...connection, entry: connection.entry || seen?.entry || '' }, actions }}>{children}</WorkspaceProvider>
}

function Live({ children }: { children: ReactNode }) {
  const device = useDevice()
  const shared = useShared()
  const { session, connection } = useConnection()
  const latest = useRef(shared)
  latest.current = shared
  const me = device.identity?.deviceId
  const { data, conversationId, nodes, questions, notifications, answered } = shared
  // Until the first read lands the list keeps what the phone last saw instead of going blank.
  const seen = useMemo(() => lastSeen(me), [me])

  const conversations: Record<string, Conversation> = {}
  if (data && conversationId) {
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
      && !jobs.some((job) => job.trigger === progress.trigger && (!busyStates.includes(job.state) || job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))))
    // The user message it answers, as the final reply's in_reply_to names it, so the two share a row: the one still
    // sending, or else the latest in the history.
    const liveTrigger = live && (sending ? shared.messagePending?.message_id : data.messages.findLast((message) => message.role === 'user')?.message_id)
    const timeline = chatTimeline(data.messages, data.questions, new Set(jobs.filter((job) => job.state === 'steered').map((job) => job.message_id)))
    // An answer is a user message, shown inside its question instead of as a bubble, as on the web.
    const folded = foldAnswers(data.messages, data.questions)
    const unread = firstUnread(timeline, shared.fresh, (question) => awaitsAnswer(question, answered, folded.answers))
    const messages: ChatMessage[] = timeline.messages.map((message) => {
      const detail = message.status === 'failed' ? jobs.find((job) => job.reply_message_id === message.message_id)?.failure?.detail : undefined
      return detail ? { ...message, detail } : message
    })
    for (const reply of shared.interrupted) messages.push({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: Date.now(), state: 'interrupted' })
    const pending = shared.messagePending
    if (pending && !data.messages.some((message) => message.message_id === pending.message_id)) messages.push({ message_id: pending.message_id, event_hash: pending.message_id, role: 'user', text: pending.text, created_at_ms: Date.now(), state: 'sending' })
    if (live) messages.push({ message_id: `live:${live.key}`, event_hash: `live:${live.key}`, role: 'assistant', text: live.text, created_at_ms: Date.now(), state: 'streaming', in_reply_to: liveTrigger })
    const stopJob = jobs.find((job) => data.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
    conversations[conversationId] = {
      messages, before: timeline.before, rest: timeline.rest, answers: folded.answers, hidden: folded.hidden, works: data.works,
      unread: unread ? 'question_id' in unread ? `q:${unread.question_id}` : unread.message_id : null,
      // A finished job whose reply has not synced yet still counts, so the activity row hands over to the reply instead of
      // vanishing a frame early; its trigger being shown, even still sending, means the newer reply is on its way, not paged out.
      replying: Boolean(live) || !handedOver && jobs.some((job) => busyStates.includes(job.state) && !(job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))
        || job.reply_message_id && job.message_id
        && !data.messages.some((message) => message.message_id === job.reply_message_id) && messages.some((message) => message.message_id === job.message_id)),
      status: jobs.some((job) => job.state === 'cancelling') ? '正在停止…' : shared.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status ?? null,
      stoppable: Boolean(stopJob && shared.writable),
      blocked: shared.readOnlyNotice || (shared.writeNotice ? '同步中，稍后可发送' : '') || composerHint({ hasConversation: true, loaded: true, bot: botReach(nodes) }) || null,
      sending: shared.messageBusy,
    }
  }

  const unread = unreadCounts(notifications, questions.filter((question) => awaitsAnswer(question, answered)))

  const directory: Member[] = (device.directory?.members ?? []).map((member) => {
    const node = nodes.find((item) => item.device_id === member.device_id), entry = member.device_id === device.ready?.entry.device_id
    return {
      device_id: member.device_id, name: member.name, kind: member.kind === 'node' ? 'node' : 'client', admin: member.manage,
      online: deviceOnline(member.kind, node, entry, shared.connectionOnline),
      current: member.device_id === me, entry, bot: botReadiness(node),
    }
  })

  const select = async (id: string) => {
    const workspace = latest.current
    const bot = workspace.bots.find((item) => item.conversation_id === id)
    if (!bot) throw new Error(botCopy.notFound)
    if (workspace.conversationId !== id) await workspace.selectBot(bot)
  }
  // A chat opened from the kept list before the first read finds no bot yet; it is selected once its bot arrives.
  const waiting = useRef<string | null>(null)
  const arrived = waiting.current !== null && shared.bots.some((bot) => bot.conversation_id === waiting.current)
  useEffect(() => {
    const id = waiting.current
    if (!arrived || !id) return
    waiting.current = null
    void select(id).catch(() => {})
  }, [arrived])
  const actions = useMemo<WorkspaceActions>(() => {
    return {
      join: (invitation, name) => device.join(invitation, name),
      cancelJoin: device.cancelJoin,
      open(id) {
        let closed = false
        latest.current.setView('chat')
        void select(id).catch((cause) => { if (!closed && cause instanceof Error && cause.message === botCopy.notFound) waiting.current = id })
        return () => { closed = true; waiting.current = null; latest.current.setView('other') }
      },
      async send(id, text, attachments) {
        await select(id)
        await latest.current.sendMessage(text, attachments.map((file) => ({ name: file.name, arrayBuffer: () => new File(file.uri).arrayBuffer() })))
      },
      async stop(id) {
        await select(id)
        const job = latest.current.data?.scheduler?.jobs.find((item) => ['queued', 'claimed', 'running'].includes(item.state))
        if (job) await latest.current.cancelManager(job.trigger)
      },
      answer: (question, text) => latest.current.answer(question, text),
      leave: device.leave,
    }
  }, [device.join, device.cancelJoin, device.leave])

  useEffect(() => { if (shared.error) console.warn(shared.error) }, [shared.error])

  const fresh = shared.synced || shared.bots.length > 0
  const bots = fresh || !seen ? shared.bots : seen.bots, primary = fresh || !seen ? shared.primary : seen.primary
  const members = device.directory || !seen ? directory : seen.members
  const snapshot = me && shared.synced && device.directory ? { me, entry: connection.entry, bots: shared.bots.map((bot) => ({ ...bot, busy: null })), primary: shared.primary, members: directory } : null
  const record = snapshot && JSON.stringify(snapshot)
  useEffect(() => { if (snapshot) writeStorage('last-seen', snapshot) }, [record])

  const value: Workspace = {
    session, connection: { ...connection, entry: connection.entry || seen?.entry || '' }, bots, primary, unread, questions, members, conversations, actions,
  }
  return <SharedContext.Provider value={shared}><WorkspaceProvider value={value}>{children}</WorkspaceProvider></SharedContext.Provider>
}
