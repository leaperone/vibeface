import { createContext, useContext, useEffect, useMemo, useRef, type ReactNode } from 'react'
import { File } from 'expo-file-system'
import { threadReplies } from '@vibeface/client/chat/protocol.ts'
import { accessOf } from '@vibeface/client/identity/access.ts'
import { DeviceProvider, useDevice } from '@vibeface/client/identity/context.tsx'
import { WorkspaceProvider as SharedWorkspace, useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { addressName } from '@vibeface/client/workspace/labels.ts'
import { mobilePlatform } from '@/platform'
import { WorkspaceProvider } from './context'
import type { ChatMessage, Connection, Conversation, Member, Session, Workspace, WorkspaceActions } from './types'

const busyStates = ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling']
const empty = { bots: [], primary: null, unread: {}, questions: [], members: [], conversations: {} }
const noTabs = { leader: () => true, post() {}, listen: () => () => {} }

type Shared = ReturnType<typeof useShared>
const SharedContext = createContext<Shared | null>(null)

/** The shared workspace while a member connection is up, for screens that call its actions directly; null before that. */
export const useLive = () => useContext(SharedContext)

/** The real network behind the screens: the shared device and workspace providers, reshaped for `useWorkspace`. */
export function LiveWorkspace({ children }: { children: ReactNode }) {
  return <DeviceProvider platform={mobilePlatform}><Device>{children}</Device></DeviceProvider>
}

function useConnection(): { session: Session; connection: Connection } {
  const device = useDevice()
  const access = accessOf({ ready: Boolean(device.ready), phase: device.phase, joining: Boolean(device.joining), savedEntries: device.entries.length, invitation: false, offlineRetryMs: null })
  const session: Session = access.kind === 'member' || access.kind === 'resuming' || access.kind === 'loading' && device.entries.length ? { phase: 'ready' }
    : device.joining ? { phase: 'joining', deviceName: device.joining.name, code: device.joining.code ?? null }
    : { phase: 'unjoined' }
  const { link } = device
  const state: Connection['state'] = link.kind === 'online' ? (link.catchingUp ? 'syncing' : 'online') : link.kind === 'offline' ? 'offline' : 'connecting'
  const entry = device.ready?.entry ?? device.entry
  return { session, connection: { state, entry: device.ready?.info.name ?? (entry ? device.entryName(entry) || addressName(entry.address) : '') } }
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
  const actions = useMemo<WorkspaceActions>(() => ({
    join: (invitation, name) => device.join(invitation, name),
    cancelJoin: device.cancelJoin,
    open: () => () => {},
    send: async () => { throw new Error('还没连上') },
    stop: async () => {},
    answer: async () => { throw new Error('还没连上') },
    disconnect: device.disconnect,
  }), [device])
  return <WorkspaceProvider value={{ ...empty, session, connection, actions }}>{children}</WorkspaceProvider>
}

function Live({ children }: { children: ReactNode }) {
  const device = useDevice()
  const shared = useShared()
  const { session, connection } = useConnection()
  const latest = useRef(shared)
  latest.current = shared
  const me = device.identity?.deviceId
  const { data, conversationId, nodes, questions, notifications, answered } = shared

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
    const messages: ChatMessage[] = threadReplies(data.messages)
    for (const reply of shared.interrupted) messages.push({ message_id: `interrupted:${reply.trigger}`, event_hash: `interrupted:${reply.trigger}`, role: 'assistant', text: reply.text, created_at_ms: Date.now(), state: 'interrupted' })
    const pending = shared.messagePending
    if (pending && !data.messages.some((message) => message.message_id === pending.message_id)) messages.push({ message_id: pending.message_id, event_hash: pending.message_id, role: 'user', text: pending.text, created_at_ms: Date.now(), state: 'sending' })
    if (live) messages.push({ message_id: `live:${live.key}`, event_hash: `live:${live.key}`, role: 'assistant', text: live.text, created_at_ms: Date.now(), state: 'streaming', in_reply_to: liveTrigger })
    const agentReady = nodes.some((node) => node.online && !node.revoked && node.capabilities?.manager)
    const stopJob = jobs.find((job) => data.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
    conversations[conversationId] = {
      messages, questions: data.questions, works: data.works,
      // A finished job whose reply has not synced yet still counts, so the activity row hands over to the reply instead of
      // vanishing a frame early; its trigger being shown, even still sending, means the newer reply is on its way, not paged out.
      replying: Boolean(live) || !handedOver && jobs.some((job) => busyStates.includes(job.state) && !(job.reply_message_id && data.messages.some((message) => message.message_id === job.reply_message_id))
        || job.reply_message_id && job.message_id
        && !data.messages.some((message) => message.message_id === job.reply_message_id) && messages.some((message) => message.message_id === job.message_id)),
      status: jobs.some((job) => job.state === 'cancelling') ? '正在停止…' : shared.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status ?? null,
      stoppable: Boolean(stopJob && shared.writable),
      blocked: shared.readOnlyNotice || (shared.writeNotice ? '同步中，稍后可发送' : '')
        || (!agentReady ? (nodes.some((node) => !node.revoked && !node.online && node.capabilities?.manager) ? '能运行 Bot 的设备离线了' : '还没设置运行 Bot 的设备') : '') || null,
      sending: shared.messageBusy,
    }
  }

  const unread: Record<string, number> = {}
  for (const item of notifications) unread[item.conversation_id] = (unread[item.conversation_id] ?? 0) + 1
  for (const question of questions) if (question.conversation_id && !question.answer && !(question.question_id in answered)) unread[question.conversation_id] = (unread[question.conversation_id] ?? 0) + 1

  const members: Member[] = (device.directory?.members ?? []).map((member) => ({
    device_id: member.device_id, name: member.name, kind: member.kind === 'node' ? 'node' : 'client', admin: member.manage,
    online: member.kind === 'node' ? nodes.some((node) => node.device_id === member.device_id && node.online) : member.device_id === me,
    current: member.device_id === me, entry: member.device_id === device.ready?.entry.device_id,
  }))

  const actions = useMemo<WorkspaceActions>(() => {
    const select = async (id: string) => {
      const workspace = latest.current
      const bot = workspace.bots.find((item) => item.conversation_id === id)
      if (!bot) throw new Error('没有这个 Bot')
      if (workspace.conversationId !== id) await workspace.selectBot(bot)
    }
    return {
      join: (invitation, name) => device.join(invitation, name),
      cancelJoin: device.cancelJoin,
      open(id) {
        latest.current.setView('chat')
        void select(id).catch(() => {})
        return () => latest.current.setView('other')
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
      disconnect: device.disconnect,
    }
  }, [device.join, device.cancelJoin, device.disconnect])

  useEffect(() => { if (shared.error) console.warn(shared.error) }, [shared.error])

  const value: Workspace = {
    session, connection, bots: shared.bots, primary: shared.primary, unread, questions, members, conversations, actions,
  }
  return <SharedContext.Provider value={shared}><WorkspaceProvider value={value}>{children}</WorkspaceProvider></SharedContext.Provider>
}
