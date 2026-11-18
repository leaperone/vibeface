import { decodeOutbox, verifyOutbox } from './outbox-codec.ts'
import { writeBody, writeKeySuffix, ownerBody, type SignedOperation } from './write-request.ts'
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react"
import type { ReadyConnection } from "../lib/node.ts"
import { messageOf } from "../lib/node.ts"
import { parseLogs, reportLog, type LogFilter } from "../lib/web-log.ts"
import { enqueue, settle, OutboxCorruptError, outboxCorruptNotice, type OutboxItem, type OutboxStore, type OutboxRecord } from "../lib/outbox.ts"
import { base64ToBytes, bytesToBase64, sha256Hex } from "../lib/byte-codec.ts"
import { requestKey } from "../lib/secure.ts"
import { notificationForOpening, type Platform } from "../lib/platform.ts"
import type { Tabs } from "../identity/context.tsx"
import { mergeProgress, type LiveProgress } from "../lib/progress.ts"
import { inputTargetKey, inputTaskCommand, isQueuedReceipt, prepareSend, retryText } from "../chat/protocol.ts"
import { taskActive } from "./labels.ts"
import { outcomeUnknown } from "../lib/errors.ts"
import { byRank, unreadReply } from "../chat/timeline.ts"
import {
  inputText, parseConversation, parseNodeReport, parseNotifications,
  parseBot, parseBots, parseCreateBot, parsePrimary, parsePendingBot, record,
  parseArtifactPage, parseBinaryPage, parseWriteStatus, parseQuestions, parseAnswer, parseOpenWorks, parseRecords, parseWorkspaceOverview, attachmentName, signedOrder, attachmentLimit,
  parseInputReceipt, parseInputNotSaved, parsePendingMessage, parseScopeContext, parseWorkDetail, parseWorkspaceCommand, parseWorkspaceCommandResult, parseWorkspaceCommandNotSaved, parseWorkspaceHistory, parseWorkspaceState,
} from "./model.ts"
import type { Attachment } from "../chat/types.ts"
import type { Avatar } from "../avatar/face.ts"
import { avatarWrite } from "../avatar/pick.ts"
import { mergeConversation, mergeWorkspaceState } from "./records.ts"
import { HistoryRequests, mergeHistoryPage, readConversationHistory } from "./history.ts"
import { createConversationCache } from "./conversation-cache.ts"
import { ConversationReadOwner, markVisibleMessagesRead, readConversationSnapshot } from "./conversation-read.ts"
import { notificationPreferences, systemNotifications } from "./system-notifications.ts"
import { answerId } from "./questions.ts"
import { memoryController, memoryResultLabel, memorySaved } from "./memory.ts"
import { networkKey, newId, storageOf } from "./storage.ts"
import type {
  HistorySection, AnswerResult, ConversationPayload, MemoryBase, MemoryControlResult, NodeInfo, NodeReportSnapshot, NotificationRecord, PendingAnswer, Question,
  PendingMessage, WorkspaceView, BotRecord, CreateBotRequest, PendingBot, BotProject, PendingCheck, WriteState, WriteStatus, OpenWork, ScheduleAction, WorkspaceOverview,
  InputControlAction, InputOptions, InputReceipt, InputNotSaved, InputTarget, WorkDetailWork, WorkspaceCommand, WorkspaceCommandResult, WorkspaceCommandNotSaved, WorkspaceDomainState, WorkspaceFilter,
} from "./types.ts"
import { isMainNotification, isReplyEnded, isSessionNotification, isTerminalJobState } from "./types.ts"

const writeNotices: Record<Exclude<WriteState, "writable">, string> = {
  aligning: "正在和你的其他设备同步，稍后就能发送。",
  read_only: "这个节点暂时只读。去待处理确认一下就能发送。",
  forked: "这个节点的数据被复制过，已暂停发送。去待处理看看。",
}

type InterruptedReply = { trigger: string; conversation_id: string; after?: string; text: string }
type LocalTask = { target: Extract<InputTarget, { kind: 'task' | 'channel_task' }>; conversation_id: string; command?: Extract<WorkspaceCommand, { kind: 'create_task' }>; saved: boolean }

/** A file the person attached: a browser `File` or a picked document on a phone. */
type AttachmentFile = { name: string; arrayBuffer(): Promise<ArrayBuffer> }
/** `label` is a full sentence for a lone button; `state` lets a settings row word it shorter. */
type NotificationButton = { label: string; disabled: boolean; state: "off" | "on" | "unsupported" | "denied" }
const soleTab: Tabs = { leader: () => true, post() {}, listen: () => () => {} }
const emptyNodes: NodeInfo[] = []
const faceSuperseded = "Bot 的长相刚在别处改过，之前没确认的那次修改没有再发送。"
const inputMissingReason = '这条输入的送达结果仍未确认，原内容已保留。请连接原设备，再确认原设备并查询。'
const responded = (caught: unknown) => caught instanceof Error && "responded" in caught && caught.responded === true
class UnconfirmedInputTargetError extends Error {
  constructor() { super('本地输入目标尚未确认，已保留原输入。请重新读取工作区。') }
}
class WorkspaceCommandNotSavedError extends Error {
  constructor(receipt: WorkspaceCommandNotSaved) { super(receipt.reason || '这次操作未保存。请读取最新状态后再操作。') }
}

function useWorkspaceState(platform: Platform, connection: ReadyConnection, openConversation: (conversationId: string, target?: InputTarget) => void = () => {}, tabs: Tabs = soleTab, initialBot: string | null = null) {
  const { notifier } = platform
  const inputLock: NonNullable<Platform["inputLock"]> = platform.inputLock ?? ((_name, save) => save())
  const { readStorage, writeStorage } = storageOf(platform.storage)
  const connectionGeneration = connection.generation
  const networkId = connection.principal.network_id
  const clientId = connection.principal.device_id
  const systemAlerts = useMemo(() => systemNotifications({ storage: platform.storage, networkId, notifier }), [platform.storage, networkId, notifier])
  /** A local session reads this computer's records, but the network takes only a member's signature. */
  const joined = connection.principal.source === "membership"
  const prefix = `${networkId}.${clientId}:`
  const [ledgerFailed, setLedgerFailed] = useState(false)
  const ledgerFailure = useRef(false)
  const ledgerLease = useRef(platform.outbox.lease?.()).current
  const ledgerGuard = () => {
    requireCurrent()
    ledgerLease?.()
    platform.outbox.guard?.()
    if (ledgerFailure.current) throw new OutboxCorruptError()
  }
  const corruptLedger = (): never => {
    ledgerFailure.current = true
    if (current()) { setLedgerFailed(true); setBooted(false); setError({ text: outboxCorruptNotice }) }
    if (platform.outbox.corrupt) return platform.outbox.corrupt()
    throw new OutboxCorruptError()
  }
  const outbox: OutboxStore = {
    guard: ledgerGuard, lease: () => ledgerGuard,
    all: async () => {
      ledgerGuard()
      try {
        const items = (await platform.outbox.all()).map(item => decodeOutbox(item, item.key))
        await verifyOutbox(items, platform.secure)
        ledgerGuard()
        const scopedItems = items.filter(item => item.key.startsWith(prefix))
        if (loadedMessageConnection.current === connection) for (const item of scopedItems) {
          if (item.method !== 'message') continue
          const target = item.params.target
          const command = item.prerequisite?.params.command
          const ownerId = command?.kind === 'create_task' ? command.owner_bot : 'bot_id' in target ? target.bot_id : undefined
          if (ownerId === undefined) continue
          const owner = botsRef.current.find(bot => bot.bot_id === ownerId)
          if (owner && owner.conversation_id !== item.scope_id) corruptLedger()
        }
        return scopedItems
      } catch (cause) { if (cause instanceof OutboxCorruptError) corruptLedger(); throw cause }
    },
    put: async item => { ledgerGuard(); if (!item.key.startsWith(prefix)) corruptLedger(); await platform.outbox.put(item); ledgerGuard() },
    delete: async key => { ledgerGuard(); if (!key.startsWith(prefix)) corruptLedger(); await platform.outbox.delete(key); ledgerGuard() },
  }
  const outboxKey = (kind: string, id: string) => `${prefix}${kind}:${id}`
  const inputDeliveries = useRef(new Map<string, Promise<unknown>>())
  const withInputDelivery = async <T,>(lane: string, deliver: () => Promise<T>): Promise<T> => {
    const key = outboxKey('input-delivery', lane)
    const pending = (inputDeliveries.current.get(key) ?? Promise.resolve()).catch(() => {}).then(() => inputLock(key, deliver))
    inputDeliveries.current.set(key, pending)
    try { return await pending }
    finally { if (inputDeliveries.current.get(key) === pending) inputDeliveries.current.delete(key) }
  }
  /** This device's signature over the exact event body the Node will write for it. */
  const attest = async (scope: string, body: Record<string, unknown>) => {
    await outbox.all(); ledgerGuard()
    const signature = await platform.transport.signClient("attestation", platform.secure.attestationPayload(networkId, scope, JSON.stringify(body)))
    ledgerGuard()
    return { device_id: clientId, signature }
  }
  const [booted, setBooted] = useState(false)
  const [error, setError] = useState<{ text: string; retry?: () => void } | null>(null)
  const [noticeText, setNoticeText] = useState("")
  const [view, setView] = useState<WorkspaceView>("other")
  // Read synchronously: the chat route selects its conversation in the same commit that sets the view.
  const viewRef = useRef<WorkspaceView>("other")
  const [nodeState, setNodeState] = useState<{ connection: ReadyConnection; generation: number; report: NodeReportSnapshot } | null>(null)
  const nodes = nodeState?.report.nodes ?? emptyNodes
  const nodeReport = nodeState?.connection === connection && nodeState.generation === connectionGeneration ? nodeState.report : null
  const nodeRequest = useRef<{ connection: ReadyConnection; generation: number; read: Promise<void> } | null>(null)
  const nodesRef = useRef(nodes)
  nodesRef.current = nodes
  const [writeState, setWriteState] = useState<WriteStatus>({ state: "writable", conflicts: [] })
  const [conversationId, setConversationId] = useState("")
  const [data, setData] = useState<ConversationPayload | null>(null)
  const [dataOrigin, setDataOrigin] = useState<'none' | 'cache' | 'network'>('none')
  const [conversationLoading, setConversationLoading] = useState(false)
  const [conversationError, setConversationError] = useState<string | null>(null)
  const conversationCache = useMemo(() => platform.conversationCache ? createConversationCache({ storage: platform.storage }) : null, [platform.storage, platform.conversationCache])
  const [notifications, setNotifications] = useState<NotificationRecord[]>([])
  const [ignoredNotifications, setIgnoredNotifications] = useState<ReadonlySet<string>>(new Set())
  const [questions, setQuestions] = useState<Question[]>([])
  const [openWorks, setOpenWorks] = useState<OpenWork[]>([])
  const openWorksRef = useRef<OpenWork[]>([])
  const knownWorks = useRef(new Map<string, WorkDetailWork>())
  const [overview, setOverview] = useState<WorkspaceOverview | null>(null)
  const [workspaceState, setWorkspaceState] = useState<WorkspaceDomainState | null>(null)
  const workspaceStateRef = useRef<WorkspaceDomainState | null>(null)
  const workspaceReadSequence = useRef(0)
  const workspaceSource = useRef<{ connection: ReadyConnection; generation: number } | null>(null)
  /** Answers this browser saw land, by question, until the network's own reads show them. */
  const [answered, setAnswered] = useState<Readonly<Record<string, string>>>({})
  const [pendingAnswers, setPendingAnswers] = useState<PendingAnswer[]>([])
  const answersRef = useRef<PendingAnswer[]>([])
  const [pendingMessages, setPendingMessages] = useState<PendingMessage[]>([])
  const [bots, setBots] = useState<BotRecord[]>([])
  const botsRef = useRef<BotRecord[]>([])
  botsRef.current = bots
  const [primary, setPrimary] = useState<string | null>(null)
  const [pendingBot, setPendingBot] = useState<PendingBot | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const [messageBusy, setMessageBusy] = useState(false)
  const [liveProgress, setLiveProgress] = useState<LiveProgress[]>([])
  const [interrupted, setInterrupted] = useState<InterruptedReply[]>([])
  /**
   * What was on the open chat when it came on screen: its unread replies by event hash and its questions as `question:<id>`.
   * The chat marks where the unread ones start; a question asked while the chat is open is seen as it arrives.
   */
  const [fresh, setFresh] = useState<ReadonlySet<string>>(new Set<string>())
  /** The chat on screen and the connection it was read over; another connection is another visit. */
  const onScreen = useRef<{ id: string; connection: ReadyConnection } | null>(null)
  const liveRef = useRef<LiveProgress[]>([])
  liveRef.current = liveProgress
  const [notificationButton, setNotificationButton] = useState<NotificationButton>({ label: `开启${notifier.name}`, disabled: true, state: "off" }) // disabled until the first permission check
  const alive = useRef(true)
  const activeId = useRef("")
  const dataRef = useRef<ConversationPayload | null>(null)
  const dataScope = useRef({ networkId, clientId })
  const readConnection = useRef<ReadyConnection | null>(null)
  const readGeneration = useRef<number | null>(null)
  const conversationReads = useRef(new ConversationReadOwner())
  const historyRequests = useRef(new HistoryRequests())
  const [historyLoading, setHistoryLoading] = useState<ReadonlySet<HistorySection>>(new Set())
  const messagesRef = useRef<PendingMessage[]>([])
  const botCreationRef = useRef<PendingBot | null>(null)
  const notificationsRef = useRef<NotificationRecord[]>([])
  const ackNotificationRef = useRef<(notification: NotificationRecord) => Promise<void>>(async () => {})
  const ignoredNotificationsRef = useRef<ReadonlySet<string>>(new Set())
  const inFlight = useRef(new Set<string>())
  const messageSaves = useRef(new Map<string, Promise<void>>())
  const channelTasks = useRef(new Map<string, LocalTask>())
  const messageTargets = useRef(new Set<string>())
  const workspaceWrites = useRef(new Map<string, Promise<WorkspaceCommandResult>>())
  const flushMessagesRef = useRef<() => void>(() => {})
  const renderedConnection = useRef(connection)
  renderedConnection.current = connection
  const renderedGeneration = useRef(connectionGeneration)
  renderedGeneration.current = connectionGeneration
  const loadedMessageConnection = useRef<ReadyConnection | null>(null)
  const localMessageSaves = useRef(0)
  const refreshingRef = useRef(false)
  const refreshingConnection = useRef<ReadyConnection | null>(null)
  const refreshingGeneration = useRef<number | null>(null)
  const refreshAgain = useRef(false)
  const refreshPromiseRef = useRef<Promise<void> | null>(null)
  const refreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const refreshRef = useRef<() => Promise<void>>(async () => {})
  const current = useCallback(() => alive.current && connection.isCurrent() && connection.generation === connectionGeneration && connection.principal.network_id === networkId && connection.principal.device_id === clientId && renderedConnection.current === connection && renderedGeneration.current === connectionGeneration, [connection, connectionGeneration, networkId, clientId])
  // Settings and workspace refreshes share one read for this connection. Never publish a prior connection's path.
  const refreshNodes = useCallback(async () => {
    if (!current()) throw new Error("连接换了。再查一次。")
    const pending = nodeRequest.current
    if (pending?.connection === connection && pending.generation === connectionGeneration) return pending.read
    const read = connection.rpc("nodes").then((raw) => {
      if (!current()) throw new Error("连接换了。再查一次。")
      const report = { ...parseNodeReport(raw), receivedAt: Date.now() }
      setNodeState({ connection, generation: connectionGeneration, report })
    }).finally(() => {
      if (nodeRequest.current?.read === read) nodeRequest.current = null
    })
    nodeRequest.current = { connection, generation: connectionGeneration, read }
    return read
  }, [connection, connectionGeneration, current])
  const readMethods = new Set(['message_search', 'conversation_history', 'conversation', 'bots', 'nodes', 'notifications', 'questions', 'open_works', 'write_status', 'workspace_state', 'workspace_history', 'work_detail', 'scope_context', 'read_artifact', 'logs', 'history', 'workspace_overview'])
  async function rpc(method: string, params?: Record<string, unknown>, sideEffectGuard?: () => void) {
    requireCurrent()
    if (!readMethods.has(method)) {
      const pending = await outbox.all(); ledgerGuard()
      if (method === 'message') {
        const item = pending.find(item => item.method === 'message' && item.params.message_id === params?.message_id)
        if (!item || item.method !== 'message') throw new OutboxCorruptError()
        requireMessageTarget(item)
      }
      if (method === 'workspace_command') {
        const command = parseWorkspaceCommand(params?.command)
        if (command.kind === 'create_task' && !botsRef.current.some(bot => bot.bot_id === command.owner_bot)) throw new Error('任务所属的 Bot 尚未确认，已保留原操作。请重新读取工作区。')
      }
      if (method === 'upload_attachment' && !botsRef.current.some(bot => bot.conversation_id === params?.conversation_id)) throw new Error('附件所属的 Bot 尚未确认，已保留原输入。请重新读取工作区。')
    }
    sideEffectGuard?.()
    const result = await connection.rpc(method, params)
    if (!readMethods.has(method)) ledgerGuard()
    return result
  }
  const requireMessageTarget = (item: OutboxRecord<'message'>) => {
    requireCurrent()
    const fail = (): never => { throw new UnconfirmedInputTargetError() }
    if (loadedMessageConnection.current !== connection) fail()
    const target = item.params.target, command = item.prerequisite?.params.command
    let ownerId = 'bot_id' in target ? target.bot_id : undefined
    if (target.kind === 'task' || target.kind === 'channel_task') {
      const source = workspaceSource.current
      const task = source?.connection === connection && source.generation === connectionGeneration
        ? workspaceStateRef.current?.tasks.find(task => task.task_id === target.task_id) : undefined
      if (task) {
        if (target.kind === 'channel_task' ? task.channel_id !== target.channel_id || task.owner_bot !== target.bot_id : task.channel_id !== undefined) fail()
        if (command?.kind === 'create_task' && (task.owner_bot !== command.owner_bot || task.channel_id !== command.channel_id)) fail()
        ownerId = task.owner_bot
      } else if (command?.kind === 'create_task') {
        // A new task need not exist yet, but its signed prerequisite must resolve
        // through the bots read from this connection, never through pending inputs.
        ownerId = command.owner_bot
      } else fail()
    }
    if (ownerId !== undefined && !botsRef.current.some(bot => bot.bot_id === ownerId && bot.conversation_id === item.scope_id)) fail()
  }
  const sameRead = () => readConnection.current === connection && readGeneration.current === connectionGeneration && (!(platform.conversationCache || platform.explicitMessageRead) || !activeId.current || conversationReads.current.confirmed())
  const requireCurrent = () => { if (!current()) throw new Error("连接换了。再查一次。") }
  const requireContext = (id = activeId.current) => {
    requireCurrent()
    const context = dataRef.current
    if (!sameRead() || !id || id !== activeId.current || context?.conversation?.conversation_id !== id) throw new Error("先选择并读取对话。")
    return context
  }
  const requireBotContext = (id = activeId.current) => {
    if (!bots.some((bot) => bot.conversation_id === id)) throw new Error("还没选 Bot。先选一个。")
    return requireContext(id)
  }
  const showError = (caught: unknown, action?: string, retry?: () => void) => { if (current()) setError({ text: messageOf(caught, action), retry }) }
  /** A failed background read names itself and offers one more try. */
  const showRefreshError = (caught: unknown) => showError(caught, "读取最新内容", () => { void refreshRef.current().catch(showRefreshError) })
  const requireWritable = () => {
    ledgerGuard()
    if (!joined) throw new Error("本机管理不能发送。用加入链接加入后再操作。")
    if (!booted) throw new Error("本地记录还没读完，先暂停发送。")
    if (!sameRead()) throw new Error("正在补读这个节点的记录，已暂停写入。")
    if (writeState.state !== "writable") throw new Error(writeNotices[writeState.state])
  }
  const scoped = (key: string) => networkKey(`${key}.${clientId}`, networkId)
  const ignoredInboxKey = scoped("ignored-inbox")
  const readIgnoredNotifications = () => {
    const raw = readStorage(ignoredInboxKey, [])
    return new Set(stringList(raw) ?? [])
  }
  /** Unconfirmed messages and answers as the page shows them, read back from the outbox. */
  const syncPending = async () => {
    const items = await outbox.all()
    const next = items.filter((item) => item.method === "message").map(parsePendingMessage).sort((a, b) => a.created_at_ms - b.created_at_ms || a.sequence - b.sequence)
    const answers: PendingAnswer[] = items.filter((item): item is OutboxRecord<"answer"> => item.method === "answer" && item.rejected === undefined)
      .map((item) => ({ question_id: String(item.params.question_id), text: String(item.params.text) }))
    if (!current()) return
    for (const [key, task] of channelTasks.current) {
      const pending = next.some((input) => inputTargetKey(input.target) === inputTargetKey(task.target))
      const saving = messageSaves.current.has(messageLane(task))
      if (task.saved && !pending && !saving) channelTasks.current.delete(key)
    }
    for (const pending of next) {
      if (pending.target.kind !== 'channel_task') continue
      const target = pending.target, key = inputTargetKey({ kind: 'channel_member', channel_id: target.channel_id, bot_id: target.bot_id })
      if (channelTasks.current.has(key)) continue
      const tasks = next.filter((input) => input.target.kind === 'channel_task' && input.target.channel_id === target.channel_id && input.target.bot_id === target.bot_id)
      if (new Set(tasks.map((input) => inputTargetKey(input.target))).size !== 1) continue
      const stored = items.find((item) => item.method === 'message' && item.params.message_id === pending.message_id)?.prerequisite
      const command = stored ? parseWorkspaceCommand(stored.params.command) : undefined
      channelTasks.current.set(key, { target, conversation_id: pending.conversation_id, saved: true, ...(command?.kind === 'create_task' && command.task_id === target.task_id ? { command } : {}) })
    }
    messagesRef.current = next
    answersRef.current = answers
    if (alive.current) { setPendingMessages(next); setPendingAnswers(answers) }
  }
  const outboxChanged = async () => { await syncPending(); tabs.post({ type: "outbox", networkId }) }
  /** Keeps the signed request until the entry answers; an unknown outcome stays for the next check. */
  const signedWrite = async (scope_id: string, operation: SignedOperation) => {
    const key = prefix + writeKeySuffix(operation)
    const item = await enqueue(outbox, decodeOutbox({ key, ...operation, scope_id, created_at_ms: Date.now() }, key))
    try {
      const result = await rpc(item.method, item.params)
      await outbox.delete(item.key)
      return result
    } catch (caught) {
      if (responded(caught)) await outbox.delete(item.key)
      throw caught
    }
  }
  const memory = memoryController({
    outbox, key: (conversationId, memoryId) => outboxKey("memory", JSON.stringify([conversationId, memoryId])),
    rpc: (method, params) => { requireCurrent(); return rpc(method, params) },
    sign: async (scope, body) => {
      await outbox.all(); ledgerGuard()
      const payload = platform.secure.attestationPayload(networkId, scope, JSON.stringify(body))
      const signature = await platform.transport.signClient("attestation", payload)
      requireCurrent()
      ledgerGuard()
      return { attestation: { device_id: clientId, signature }, operation_id: await requestKey(payload, clientId) }
    },
  })
  const persistConversation = (id: string, payload: ConversationPayload) => {
    if (!conversationCache) return
    try { conversationCache.save({ networkId, deviceId: clientId, conversationId: id, payload }) }
    catch (caught) { reportLog({ level: 'warn', event: 'conversation.cache_save_failed', message: messageOf(caught), fields: { conversationId: id } }) }
  }
  const cachedConversation = (id: string) => {
    if (!conversationCache || !id) return null
    try { return conversationCache.read({ networkId, deviceId: clientId, conversationId: id })?.payload ?? null }
    catch (caught) { reportLog({ level: 'warn', event: 'conversation.cache_read_failed', message: messageOf(caught), fields: { conversationId: id } }); return null }
  }
  const selectConversation = (id: string) => {
    requireCurrent()
    activeId.current = id
    if (conversationCache && id) {
      writeStorage(scoped('selected-conversation'), id)
      const bot = botsRef.current.find((item) => item.conversation_id === id)
      if (bot) writeStorage(scoped(`conversation-for-bot.${bot.bot_id}`), id)
    }
    conversationReads.current.invalidate()
    historyRequests.current.reset()
    setHistoryLoading(new Set())
    const cached = cachedConversation(id)
    dataScope.current = { networkId, clientId }
    dataRef.current = cached
    onScreen.current = null
    setFresh(new Set<string>())
    setConversationId(id)
    setData(cached)
    setDataOrigin(cached ? 'cache' : 'none')
    setConversationLoading(false)
    setConversationError(null)
    setLiveProgress([])
    setInterrupted([])
  }
  const readConversation = async (id: string) => {
    requireCurrent()
    if (!id || activeId.current !== id) return null
    setConversationError(null)
    return readConversationSnapshot({
      conversationId: id, owner: conversationReads.current,
      current: () => current() && activeId.current === id,
      read: () => rpc('conversation', { conversation_id: id }),
      shown: () => dataRef.current,
      loading: setConversationLoading,
      failed: (caught) => setConversationError(messageOf(caught, '读取对话')),
      authorityChanged: () => {
        historyRequests.current.reset()
        setHistoryLoading(new Set())
      },
      apply: (next) => showConversation(id, next),
    })
  }
  const refreshConversation = async () => { requireCurrent(); if (activeId.current) await readConversation(activeId.current) }
  const cancelHistory = (section: HistorySection) => {
    historyRequests.current.cancel(section)
    setHistoryLoading((previous) => new Set([...previous].filter((item) => item !== section)))
  }
  const loadHistory = async (section: HistorySection) => {
    const shown = requireContext(), id = activeId.current, generation = connection.generation
    if (shown.history[section].complete) return
    const request = historyRequests.current.start(section)
    if (!request) return
    setHistoryLoading((previous) => new Set([...previous, section]))
    let expectedCursor = shown.history[section].next_cursor
    const valid = () => request.current() && current() && renderedConnection.current === connection && connection.generation === generation && activeId.current === id && dataRef.current?.history_authority === shown.history_authority && dataRef.current.history[section].next_cursor === expectedCursor
    try {
      await readConversationHistory({ rpc: (method, params) => rpc(method, params), conversationId: id, section, cursor: shown.history[section].next_cursor, authority: shown.history_authority, current: valid, apply: (page) => {
        const latest = dataRef.current
        if (!latest) return
        const next = mergeHistoryPage(latest, page)
        expectedCursor = page.next_cursor
        if (page.section === 'messages') conversationReads.current.confirm(page.items)
        dataRef.current = next
        setData(next)
        persistConversation(id, next)
      } })
    } catch (caught) {
      if (!valid()) return
      if (messageOf(caught).includes('refresh the conversation')) {
        historyRequests.current.reset()
        setHistoryLoading(new Set())
        conversationReads.current.invalidate()
        readConnection.current = null
        if (conversationCache) setDataOrigin('cache')
        else { dataRef.current = null; setData(null); setDataOrigin('none') }
        scheduleRefresh()
        await refreshRef.current()
        return
      }
      throw caught
    } finally {
      if (request.current()) cancelHistory(section)
      request.finish()
    }
  }
  const notificationForMain = (notification: NotificationRecord, id: string) => notification.conversation_id === id && isMainNotification(notification, botsRef.current.find((bot) => bot.conversation_id === id)?.bot_id)
  const acknowledgeNotification = async (notification: NotificationRecord) => {
    await signedWrite(notification.conversation_id, { method: "ack", params: { notification_id: notification.notification_id, attestation: await attest(notification.conversation_id, writeBody({ method: "ack", params: { notification_id: notification.notification_id } }, clientId)) } })
    requireCurrent()
    notificationsRef.current = notificationsRef.current.filter((item) => item.notification_id !== notification.notification_id)
    setNotifications(notificationsRef.current)
  }
  const showConversation = async (id: string, next: ConversationPayload) => {
    // While the entry catches up, what the page already showed stays on screen.
    const shown = dataRef.current
    if (connection.catchingUp?.() && shown?.conversation?.conversation_id === id && shown.history_authority === next.history_authority) {
      const missing = shown.messages.filter((message) => !next.messages.some((item) => item.message_id === message.message_id))
      if (missing.length) next = { ...next, messages: byRank([...next.messages, ...missing]) }
    }
    dataRef.current = next
    setData(next)
    setDataOrigin('network')
    persistConversation(id, next)
    if (viewRef.current !== "chat" || !platform.foreground()) onScreen.current = null
    else if (onScreen.current?.id !== id || onScreen.current.connection !== connection) {
      onScreen.current = { id, connection }
      setFresh(new Set([...notificationsRef.current.filter((item) => notificationForMain(item, id) && unreadReply(item)).map((item) => item.notification_id), ...next.questions.map((question) => `question:${question.question_id}`)]))
    }
    // A message or an answer settles once the Node's own history holds it.
    const admitted = (item: PendingMessage) => item.rejection_code !== 'input_conflict' && item.receipt?.stage !== 'refused' && item.receipt?.stage !== 'unknown' && item.conversation_id === id && next.messages.some((message) => message.message_id === item.message_id && conversationReads.current.observedMessage(message))
    const landed = answersRef.current.filter((item) => next.messages.some((message) => message.message_id === answerId(item.question_id) && conversationReads.current.observedMessage(message)))
    if (current() && (messagesRef.current.some(admitted) || landed.length)) {
      for (const item of messagesRef.current.filter(admitted)) await withInputDelivery(messageLane(item), async () => {
        if (!current()) return
        const latest = (await outbox.all()).find((entry) => entry.key === outboxKey('message', item.message_id))
        if (current() && latest && admitted(parsePendingMessage(latest))) await outbox.delete(latest.key)
      })
      for (const item of landed) await outbox.delete(outboxKey("answer", item.question_id))
      await outboxChanged()
    }
    if (!current() || activeId.current !== id || dataRef.current !== next) return next
    // Streamed text of a reply that ended without a final message is kept locally and shown as interrupted.
    const ended = (job: { state: string; reply_message_id?: string } | undefined) => job && isReplyEnded(job.state) && !(job.reply_message_id && next.messages.some((message) => message.message_id === job.reply_message_id))
    const kept = interruptedReplies(readStorage(scoped("interrupted"), [])).filter((item) => item.conversation_id !== id || next.scheduler?.jobs.some((job) => job.trigger === item.trigger && ended(job)))
    for (const progress of liveRef.current) {
      if (progress.kind !== "manager" || !progress.text || progress.conversation_id !== id || kept.some((item) => item.trigger === progress.trigger)) continue
      const job = next.scheduler?.jobs.find((item) => item.trigger === progress.trigger)
      if (ended(job)) kept.push({ trigger: progress.trigger, conversation_id: id, after: job?.message_id, text: progress.text })
    }
    writeStorage(scoped("interrupted"), kept.slice(-50))
    setInterrupted(kept.filter((item) => item.conversation_id === id))
    setLiveProgress((previous) => previous.filter((progress) => {
      if (progress.kind === "native") return next.works.some((item) => item.target_node === progress.target_node && item.work_id === progress.work_id && item.state !== "finished")
      const job = next.scheduler?.jobs.find((item) => item.trigger === progress.trigger)
      return Boolean(job && (!job.reply_message_id || !next.messages.some((message) => message.message_id === job.reply_message_id)) && !isReplyEnded(job.state))
    }))
    if (!platform.explicitMessageRead && joined && current() && sameRead() && viewRef.current === "chat" && platform.foreground()) {
      const seen = notificationsRef.current.filter((item) => notificationForMain(item, id) && !isSessionNotification(item))
      for (const notification of seen) {
        if (!current() || activeId.current !== id || !sameRead() || viewRef.current !== 'chat' || !platform.foreground()) break
        try { await acknowledgeNotification(notification) } catch (caught) { showError(caught, "标记已读"); break }
      }
    }
    return next
  }
  const refreshNotificationButton = async () => {
    const permission = await notifier.permission()
    if (!current()) return
    const enabled = readStorage(networkKey("notify", networkId), false) === true
    setNotificationButton(permission === "unsupported" ? { label: `不支持${notifier.name}`, disabled: true, state: "unsupported" }
      : permission === "granted" ? { label: enabled ? `${notifier.name}已开启` : "启用此网络提醒", disabled: enabled, state: enabled ? "on" : "off" }
      : permission === "denied" ? { label: `在设置里允许${notifier.name}`, disabled: true, state: "denied" }
      : { label: `开启${notifier.name}`, disabled: false, state: "off" })
  }
  const notify = async (items: NotificationRecord[]) => {
    await systemAlerts.deliver({
      items, current,
      eligible: (item) => !isSessionNotification(item) || platform.sessionNotifications !== false,
      visible: (item) => viewRef.current === "chat" && notificationForMain(item, activeId.current) && platform.foreground(),
      show: (item) => notifier.show({ title: item.kind === "question" ? "Vibeface · 需要你回答" : isSessionNotification(item) ? "Vibeface · 接手会话" : "Vibeface · 进展", body: item.text, tag: item.notification_id, destination: { network_id: networkId, device_id: clientId, notification_id: item.notification_id, conversation_id: item.conversation_id } }, () => {
          if (!current()) return
          if (isSessionNotification(item)) {
            void ackNotificationRef.current(item).then(() => { if (current()) openConversation(item.conversation_id, item.target) }, () => {})
          } else {
            openConversation(item.conversation_id, item.target)
          }
      }),
    })
  }
  useEffect(() => notifier.watchOpen?.(async (destination) => {
    try {
      const result = await notificationForOpening({ destination, connection, current, read: async () => parseNotifications(await rpc("notifications")) })
      if (result.kind === 'deferred') return false
      if (result.kind === 'invalid') return true
      const item = result.notification
      if (isSessionNotification(item) && platform.sessionNotifications === false) return true
      if (isSessionNotification(item)) await ackNotificationRef.current(item)
      if (!current()) return false
      openConversation(item.conversation_id, item.target)
      return true
    } catch { return false }
  }), [connection])
  useEffect(() => { notifier.replayOpen?.() }, [notifications])
  /** A device starts with the history it found read: what waited on its first full read never counts as unread here. */
  const unreadSince = (items: NotificationRecord[], first = false) => {
    const key = networkKey(`read-before.${clientId}`, networkId)
    const before = stringList(readStorage(key, null))
    if (!before && first) { writeStorage(key, items.map((item) => item.notification_id)); return [] }
    return before ? items.filter((item) => !before.includes(item.notification_id)) : items
  }
  const scheduleRefresh = () => {
    if (!current() || refreshTimer.current !== null) return
    refreshTimer.current = setTimeout(() => {
      refreshTimer.current = null
      void refreshRef.current().catch(showRefreshError)
    }, 350)
  }
  const refresh = async () => {
    requireCurrent()
    if (refreshingRef.current && refreshingConnection.current === connection && refreshingGeneration.current === connectionGeneration) {
      refreshAgain.current = true
      const pending = refreshPromiseRef.current
      if (pending) await pending
      return
    }
    let resolveRefresh: () => void = () => {}, rejectRefresh: (caught: unknown) => void = () => {}
    const refreshPromise = new Promise<void>((resolve, reject) => { resolveRefresh = resolve; rejectRefresh = reject })
    // The operation is also awaited by explicit retries; mark its rejection handled for callers that only schedule refresh.
    refreshPromise.catch(() => {})
    refreshPromiseRef.current = refreshPromise
    refreshingRef.current = true
    refreshingConnection.current = connection
    refreshingGeneration.current = connectionGeneration
    setRefreshing(true)
    try {
      const readingId = activeId.current
      const conversationRead = conversationCache && readingId ? readConversation(readingId) : null
      // The metadata read may take longer; retain the read error until this refresh awaits it.
      conversationRead?.catch(() => {})
      const [, rawNotifications, rawBots, rawWrite, rawQuestions, rawWorks, rawOverview] = await Promise.all([
        refreshNodes(), rpc("notifications"), rpc("bots"), connection.request("write_state"), rpc("questions"), rpc("open_works"),
        viewRef.current === "tasks" ? rpc("workspace_overview") : null,
      ])
      requireCurrent()
      const parsedNotifications = parseNotifications(rawNotifications)
      const nextNotifications = platform.explicitMessageRead ? parsedNotifications : unreadSince(parsedNotifications, true)
      const nextBots = parseBots(rawBots), nextQuestions = parseQuestions(rawQuestions)
      openWorksRef.current = parseOpenWorks(rawWorks)
      if (rawOverview !== null) setOverview(parseWorkspaceOverview(rawOverview))
      botsRef.current = nextBots
      setNotifications(nextNotifications); setBots(nextBots); setPrimary(parsePrimary(rawBots)); setWriteState(parseWriteStatus(rawWrite)); setQuestions(nextQuestions); setOpenWorks(openWorksRef.current)
      if (botCreationRef.current && nextBots.some((item) => item.bot_id === botCreationRef.current?.bot_id)) clearPendingBot()
      notificationsRef.current = nextNotifications
      if (platform.explicitMessageRead && viewRef.current === 'chat' && platform.foreground()) {
        setFresh((previous) => new Set([...previous, ...nextNotifications.filter((item) => notificationForMain(item, activeId.current) && unreadReply(item)).map((item) => item.notification_id)]))
      }
      const selectedValue = readStorage(scoped("selected-bot"), null)
      const selected = typeof selectedValue === "string" ? selectedValue : null
      const bot = nextBots.find((item) => item.conversation_id === activeId.current) ?? nextBots.find((item) => item.bot_id === selected) ?? nextBots[0]
      if (activeId.current !== (bot?.conversation_id ?? "") && !(conversationCache && !bot && activeId.current && connection.catchingUp?.())) selectConversation(bot?.conversation_id ?? "")
      if (activeId.current) {
        if (conversationRead && activeId.current === readingId) await conversationRead
        else await readConversation(activeId.current)
      }
      requireCurrent()
      if (activeId.current && !conversationReads.current.confirmed()) {
        resolveRefresh()
        return
      }
      readConnection.current = connection
      readGeneration.current = connectionGeneration
      loadedMessageConnection.current = connection
      flushMessagesRef.current()
      void readWorkspaceState().catch(showRefreshError)
      void notify(nextNotifications.filter((item) => !ignoredNotificationsRef.current.has(item.notification_id))).catch((caught) => showError(caught, '显示系统提醒'))
      void refreshNotificationButton()
      resolveRefresh()
    } catch (caught) {
      // What the page shows stays; writes wait for the next full read.
      if (current()) readConnection.current = null
      rejectRefresh(caught)
      throw caught
    } finally {
      if (refreshingConnection.current === connection && refreshingGeneration.current === connectionGeneration) refreshingRef.current = false
      if (current()) {
        setRefreshing(false)
        if (refreshAgain.current) { refreshAgain.current = false; scheduleRefresh() }
      }
      if (refreshPromiseRef.current === refreshPromise) refreshPromiseRef.current = null
    }
  }
  refreshRef.current = refresh
  /** One pushed change merged in place; a full read only when a read races it or it does not parse. */
  const applyRecords = async (value: Record<string, unknown>) => {
    const records = parseRecords(value), id = records.conversation_id
    if (records.refresh_metadata) scheduleRefresh()
    if (id === activeId.current && dataRef.current && records.history_authority !== dataRef.current.history_authority) {
      conversationReads.current.invalidate(); historyRequests.current.reset(); setHistoryLoading(new Set())
      readConnection.current = null
      if (conversationCache) {
        setDataOrigin('cache')
        void refreshConversation().catch(showRefreshError)
      } else { dataRef.current = null; setData(null); setDataOrigin('none') }
      scheduleRefresh(); return
    }
    if (refreshingRef.current || viewRef.current === "tasks") scheduleRefresh()
    const shown = dataRef.current
    if (id === activeId.current && sameRead() && shown?.conversation?.conversation_id === id) {
      conversationReads.current.confirm(records.messages)
      await showConversation(id, mergeConversation(shown, records))
    } else if (conversationCache) {
      const retained = id === activeId.current ? shown : cachedConversation(id)
      if (retained?.history_authority !== records.history_authority) return
      const next = mergeConversation(retained, records)
      persistConversation(id, next)
      if (id === activeId.current) { dataRef.current = next; setData(next) }
    }
  }
  /** A change the Node pushed without its records: the bot list, and the conversation if it is open. */
  const reloadScope = async (id: string) => {
    if (viewRef.current === "tasks") scheduleRefresh()
    const rawBots = await rpc("bots")
    requireCurrent()
    const next = parseBots(rawBots)
    botsRef.current = next; setBots(next); setPrimary(parsePrimary(rawBots))
    if (id === activeId.current && sameRead()) await readConversation(id)
  }

  const loadPending = async () => {
    let storedBot: PendingBot | null
    try {
      const rawBot = readStorage(scoped("pending-bot"), null, true)
      storedBot = rawBot === null ? null : parsePendingBot(rawBot)
      inputSequences(readStorage(scoped('input-sequences'), {}, true))
    } catch { return corruptLedger() }
    botCreationRef.current = storedBot; setPendingBot(storedBot)
    await syncPending()
  }
  /** Is this write already in the entry's history (or moot)? Asked by business ID before any resubmission. */
  const recorded = async (item: OutboxItem) => {
    if (item.method === "ack") return !parseNotifications(await rpc("notifications")).some((note) => note.notification_id === item.params.notification_id)
    if (item.method === "create_bot") return parseBots(await rpc("bots")).some((bot) => bot.bot_id === item.params.bot_id)
    if (item.method === "delete_bot") return !parseBots(await rpc("bots")).some((bot) => bot.conversation_id === item.params.conversation_id)
    if (item.method === "set_primary_bot") return parsePrimary(await rpc("bots")) === item.params.conversation_id
    if (item.method === "set_bot_project") {
      const project = parseBots(await rpc("bots")).find((bot) => bot.conversation_id === item.params.conversation_id)?.project ?? null
      const wanted = item.params.project as BotProject | null
      return project?.target_node === wanted?.target_node && project?.cwd === wanted?.cwd
    }
    // A deleted bot has no face to set, so its write is dropped.
    if (item.method === "set_bot_avatar") {
      const bot = parseBots(await rpc("bots")).find((entry) => entry.conversation_id === item.params.conversation_id)
      if (!bot) return true
      const state = avatarWrite(bot, item.params.version as string | null, item.params.avatar as Avatar | null)
      if (state === "superseded" && current()) setNoticeText(faceSuperseded)
      return state !== "unsent"
    }
    // Only what still wants the person is listed, so a missing one is closed or answered either way.
    if (item.method === "dismiss_work") return !parseOpenWorks(await rpc("open_works")).some((work) => work.work_id === item.params.work_id)
    // Every unanswered question is listed, so one missing from the list has its answer.
    if (item.method === "answer") return !parseQuestions(await rpc("questions")).some((question) => question.question_id === item.params.question_id)
    if (item.method !== "control_schedule" && item.method !== "set_owner" && item.method !== "resolve") throw new OutboxCorruptError()
    const id = item.params.conversation_id
    const context = parseConversation(await rpc("conversation", { conversation_id: id }), id)
    if (item.method === "control_schedule") {
      // Only schedules still ahead are listed, so a missing one is deleted or done either way.
      const schedule = context.bot_settings.schedules.find((entry) => entry.schedule_id === item.params.schedule_id)
      return !schedule || item.params.action !== "delete" && schedule.enabled === (item.params.action === "resume")
    }
    if (item.method === "set_owner") return (context.conversation?.owner_epoch ?? 0) >= Number(item.params.epoch)
    if (item.method === "resolve") {
      const check = String(item.params.check_id), workId = check.startsWith("work:") ? check.slice(5) : null
      if (!workId) return !context.checks.some((entry) => entry.check_id === check)
      return context.works.some((entry) => entry.work_id === workId && entry.outcome !== null && entry.outcome !== "unverified")
    }
    throw new OutboxCorruptError()
  }
  /** After a (re)connection: confirm what the entry has, resubmit the rest byte for byte, stop at an unknown. */
  const settleOutbox = async () => {
    if (!tabs.leader() || !current()) return
    flushMessagesRef.current()
    for (const item of (await outbox.all()).filter((item) => item.method === 'workspace_command' && item.rejected === undefined)) void submitWorkspaceCommand(item).catch((caught) => showError(caught, '确认操作结果'))
    const result = await settle({ ...outbox, all: async () => (await outbox.all()).filter(item => item.method !== "control_memory" && item.method !== 'message' && item.method !== 'workspace_command') }, {
      recorded: async (item) => { requireCurrent(); return recorded(item) },
      submit: async (item) => { requireCurrent(); await rpc(item.method, item.params) },
    })
    if (!alive.current) return
    // A refused face write cannot go again: the face it was signed over has changed.
    const refusedFaces = result.rejected.filter((key) => key.startsWith(outboxKey("bot-avatar", "")))
    for (const key of refusedFaces) await outbox.delete(key)
    if (refusedFaces.length && current()) setNoticeText("之前没确认的那次长相修改被拒绝，没有生效。")
    for (const item of (await outbox.all()).filter(item => item.method === "control_memory")) {
      const key = `memory:${item.params.conversation_id}:${item.params.memory_id}`
      if (inFlight.current.has(key)) continue
      inFlight.current.add(key)
      try {
        const reconciled = await memory.reconcile(item)
        if (current() && !memorySaved(reconciled)) setNoticeText(memoryResultLabel(reconciled))
        scheduleRefresh()
      } catch (caught) { showError(caught, "查明未确认的记忆操作"); break }
      finally { inFlight.current.delete(key) }
    }
    await outboxChanged()
    if (result.confirmed.length || result.resubmitted.length || result.rejected.length) scheduleRefresh()
  }

  useEffect(() => {
    alive.current = true
    setBooted(false)
    const stopLedger = platform.outbox.watch?.(() => { try { ledgerGuard() } catch { if (current()) { setLedgerFailed(true); setBooted(false); setError({ text: outboxCorruptNotice }) } } })
    readConnection.current = null
    readGeneration.current = null
    loadedMessageConnection.current = null
    onScreen.current = null
    setFresh(new Set())
    historyRequests.current.reset()
    setHistoryLoading(new Set())
    conversationReads.current.invalidate()
    setConversationLoading(false)
    setConversationError(null)
    inFlight.current = new Set()
    messageTargets.current = new Set()
    messageSaves.current = new Map()
    workspaceWrites.current = new Map()
    localMessageSaves.current = 0
    setMessageBusy(false); setLiveProgress([]); setError(null); setNoticeText("")
    try {
      const nextIgnored = readIgnoredNotifications()
      ignoredNotificationsRef.current = nextIgnored
      setIgnoredNotifications(nextIgnored)
      if (initialBot) writeStorage(scoped("selected-bot"), initialBot)
      const storedConversation = conversationCache ? readStorage(scoped(initialBot ? `conversation-for-bot.${initialBot}` : 'selected-conversation'), null) : null
      selectConversation(typeof storedConversation === 'string' ? storedConversation : '')
      void loadPending().then(() => { if (current() && !ledgerFailure.current) setBooted(true) }, (caught) => showError(caught, "读取待发消息"))
    } catch (caught) { showError(caught) }
    const stop = tabs.listen((message) => {
      if (message.networkId !== networkId) return
      if (message.type === "outbox") { void syncPending().then(() => flushMessagesRef.current()).catch(() => {}); return }
      if (message.type !== "ignored-inbox") return
      const merged = new Set([...ignoredNotificationsRef.current, ...readIgnoredNotifications()])
      if (merged.size === ignoredNotificationsRef.current.size) return
      ignoredNotificationsRef.current = merged
      setIgnoredNotifications(merged)
    })
    return () => { alive.current = false; conversationReads.current.invalidate(); historyRequests.current.reset(); stop(); stopLedger?.() }
  }, [connection, connectionGeneration])

  useEffect(() => {
    void refreshRef.current().then(() => { if (booted && !ledgerFailed) return settleOutbox() }).catch(showRefreshError)
    const unsubscribe = connection.subscribe((event) => {
      if (!current()) return
      if (event.type === "records") { applyRecords(event.value).catch(() => scheduleRefresh()); return }
      if (event.type === "resync") { scheduleRefresh(); return }
      if (event.type === "invalidate") {
        if (event.conversation_id === undefined) scheduleRefresh()
        else reloadScope(event.conversation_id).catch(() => scheduleRefresh())
        return
      }
      if (event.conversation_id !== activeId.current) return
      const shown = dataRef.current
      const known = event.type === 'manager_progress'
        ? shown?.scheduler?.jobs.some((job) => job.scope.kind === 'main' && job.trigger === event.trigger)
        : shown?.works.some((work) => work.work_id === event.work_id && work.target_node === event.target_node)
      if (!known) { scheduleRefresh(); return }
      setLiveProgress((previous) => {
        const next = mergeProgress(previous, event)
        if (next.some((item) => item.needsRefresh && !previous.find((prior) => prior.key === item.key)?.needsRefresh)) scheduleRefresh()
        return next
      })
    })
    // A device coming or going is pushed once the Node's sync notices; this rereads sooner while one is away and catches anything missed.
    let ticks = 0
    const timer = setInterval(() => {
      ticks += 1
      if (ticks % 20 === 0) scheduleRefresh()
      else if (openWorksRef.current.length || nodesRef.current.some((node) => !node.revoked && !node.online)) void refreshNodes().catch(() => {})
    }, 15000)
    const stopForeground = platform.onForeground(scheduleRefresh)
    return () => {
      unsubscribe(); clearInterval(timer)
      if (refreshTimer.current !== null) clearTimeout(refreshTimer.current)
      refreshTimer.current = null
      stopForeground()
    }
  }, [booted, connection, connectionGeneration])

  const withMutation = async <T,>(key: string, work: () => Promise<T>) => {
    requireWritable()
    const mutations = inFlight.current
    if (mutations.has(key)) throw new Error("上一个操作还没结束。稍等一下。")
    mutations.add(key)
    try { return await work() } finally { mutations.delete(key) }
  }
  const ackNotification = (notification: NotificationRecord) => withMutation(`ack:${notification.notification_id}`, async () => {
    requireCurrent()
    if (!notificationsRef.current.some((item) => item.notification_id === notification.notification_id)) return
    await signedWrite(notification.conversation_id, { method: "ack", params: {
      notification_id: notification.notification_id,
      attestation: await attest(notification.conversation_id, writeBody({ method: "ack", params: { notification_id: notification.notification_id } }, clientId)),
    } })
    requireCurrent()
    notificationsRef.current = notificationsRef.current.filter((item) => item.notification_id !== notification.notification_id)
    setNotifications(notificationsRef.current)
  })
  ackNotificationRef.current = ackNotification
  const clearMessages = () => { if (current()) { setError(null); setNoticeText("") } }
  const run = async (action: () => Promise<void>, label?: string) => {
    clearMessages()
    if (label) reportLog({ level: "info", event: "web.action", message: label })
    try { await action() } catch (caught) {
      reportLog({ level: "warn", event: "web.action_failed", message: messageOf(caught), fields: { action: label } })
      showError(caught, label)
    }
  }
  const ignoreNotifications = (ids: readonly string[]) => {
    const allowed = new Set(notificationsRef.current.filter((item) => item.kind === "manager_attention" && ids.includes(item.notification_id)).map((item) => item.notification_id))
    if (!allowed.size) return
    // Another tab may have written since this connection last rendered; merge that value before saving.
    const next = new Set([...ignoredNotificationsRef.current, ...readIgnoredNotifications()])
    for (const id of allowed) next.add(id)
    // shortcut: keep the latest 200 local reminder IDs, revisit when ignored history needs to be unbounded.
    const saved = [...next].slice(-200)
    writeStorage(ignoredInboxKey, saved, true)
    ignoredNotificationsRef.current = new Set(saved)
    setIgnoredNotifications(ignoredNotificationsRef.current)
    tabs.post({ type: "ignored-inbox", networkId })
  }
  const markNotificationsRead = async (ids: readonly string[]) => {
    requireCurrent()
    if (!joined) throw new Error("本机管理不能标记网络提醒。用加入链接加入后再操作。")
    const selected = notificationsRef.current.filter((item) => ids.includes(item.notification_id))
    for (const notification of selected) await acknowledgeNotification(notification)
  }
  const markMessagesRead = async (messageIds: readonly string[]) => {
    const id = activeId.current
    const visible = () => current() && joined && viewRef.current === 'chat' && platform.foreground() && activeId.current === id && conversationReads.current.confirmed()
    await markVisibleMessagesRead({
      owner: conversationReads.current, messageIds, messages: dataRef.current?.messages ?? [],
      notifications: notificationsRef.current.filter((item) => notificationForMain(item, id)), conversationId: id, current: visible,
      acknowledge: async (notification) => {
        const key = `ack:${notification.notification_id}`
        if (inFlight.current.has(key) || !notificationsRef.current.some((item) => item.notification_id === notification.notification_id)) return
        inFlight.current.add(key)
        try { await acknowledgeNotification(notification) }
        finally { inFlight.current.delete(key) }
      },
    })
  }
  const showWorkspace = (next: WorkspaceDomainState | WorkspaceCommandResult, scoped = false) => {
    if (!current()) return
    const source = workspaceSource.current
    const previous = source?.connection === connection && source.generation === connectionGeneration ? workspaceStateRef.current : null
    workspaceStateRef.current = mergeWorkspaceState(previous, next, scoped)
    workspaceSource.current = { connection, generation: connectionGeneration }
    setWorkspaceState(workspaceStateRef.current)
  }
  const readWorkspaceState = async (filter: WorkspaceFilter = {}) => {
    requireCurrent()
    const sequence = ++workspaceReadSequence.current
    const next = parseWorkspaceState(await rpc('workspace_state', filter))
    requireCurrent()
    if (sequence === workspaceReadSequence.current) {
      showWorkspace(next, filter.channel_id !== undefined || filter.task_id !== undefined)
      flushMessagesRef.current()
    }
    return next
  }
  const readWorkspaceHistory = async (filter: WorkspaceFilter = {}, cursor?: string) => {
    requireCurrent()
    const next = parseWorkspaceHistory(await rpc('workspace_history', { ...filter, ...(cursor === undefined ? {} : { cursor }) }))
    requireCurrent()
    const admitted = (pending: PendingMessage) => next.events.some((event) => event.body.type === 'input_posted' && event.body.message_id === pending.message_id && event.scope_id === pending.conversation_id && inputTargetKey(event.body.target) === inputTargetKey(pending.target))
    let cleared = false
    for (const item of (await outbox.all()).filter((item) => item.method === 'message')) {
      const pending = parsePendingMessage(item)
      if (!admitted(pending)) continue
      await withInputDelivery(messageLane(pending), async () => {
        const latest = (await outbox.all()).find((entry) => entry.key === item.key)
        requireCurrent()
        if (!latest) return
        const checked = parsePendingMessage(latest)
        if (checked.rejection_code === 'input_conflict' || checked.receipt?.stage === 'refused' || checked.receipt?.stage === 'unknown' || latest.release_expected_event !== undefined || !admitted(checked)) return
        await outbox.delete(latest.key); cleared = true
      })
    }
    if (cleared) await outboxChanged()
    return next
  }
  const queueWorkspaceCommand = async (input: WorkspaceCommand, persist = true): Promise<OutboxRecord<'workspace_command'>> => {
    const parsed = parseWorkspaceCommand(input)
    const command = parsed.kind === 'post_channel_message' && !parsed.attachments.length ? (({ attachments: _, ...rest }) => rest)(parsed) : parsed
    const commandId = await requestKey(platform.secure.attestationPayload(networkId, 'workspace', JSON.stringify({ type: 'workspace_commanded', command })), clientId)
    const key = prefix + writeKeySuffix({ method: 'workspace_command', params: { command_id: commandId, command } })
    const existing = (await outbox.all()).find((item) => item.key === key)
    if (existing) { if (existing.method !== 'workspace_command') throw new OutboxCorruptError(); return existing }
    const attestation = await attest('workspace', writeBody({ method: 'workspace_command', params: { command_id: commandId, command } }, clientId))
    const item: OutboxRecord<'workspace_command'> = { key, scope_id: 'workspace', method: 'workspace_command', params: { command_id: commandId, command, attestation }, created_at_ms: Date.now() }
    if (!persist) return item
    const queued = await enqueue(outbox, item)
    if (queued.method !== 'workspace_command') throw new OutboxCorruptError()
    return queued
  }
  const submitWorkspaceCommand = (item: OutboxItem, targetGuard?: () => void) => {
    if (item.method !== 'workspace_command') throw new OutboxCorruptError()
    const running = workspaceWrites.current.get(item.key)
    if (running) return running
    const writes = workspaceWrites.current
    const submission = (async () => {
      requireCurrent()
      let sending = item
      try {
        targetGuard?.()
        if (sending.command_rejection !== undefined) {
          sending = { ...sending, command_rejection: undefined, rejected: undefined }
          await outbox.put(sending)
        }
        const result = await rpc('workspace_command', sending.params, targetGuard)
        requireCurrent()
        const rejection = parseWorkspaceCommandNotSaved(result, sending.params.command_id)
        if (rejection) {
          await outbox.put({ ...sending, command_rejection: rejection, rejected: rejection.reason })
          throw new WorkspaceCommandNotSavedError(rejection)
        }
        const receipt = parseWorkspaceCommandResult(result)
        if (receipt.command_id !== item.params.command_id) throw new Error('返回的操作和原操作不一致，内容已保留。请再查一次。')
        workspaceReadSequence.current += 1
        showWorkspace(receipt)
        targetGuard?.()
        await outbox.delete(item.key)
        return receipt
      } catch (caught) {
        if (responded(caught)) await outbox.put({ ...sending, rejected: caught instanceof Error ? caught.message : String(caught) })
        throw caught
      } finally { writes.delete(item.key) }
    })()
    writes.set(item.key, submission)
    return submission
  }
  const workspaceCommand = (command: WorkspaceCommand) => withMutation(`workspace-command:${JSON.stringify(command)}`, async () => {
    const item = await queueWorkspaceCommand(command)
    const receipt = await submitWorkspaceCommand(item)
    scheduleRefresh()
    return receipt
  })
  /** After a failed write: a refusal only rereads; an unknown outcome also pauses writes until the reread. */
  const requireControlRefresh = (caught: unknown) => {
    if (!current()) return
    if (!responded(caught)) {
      readConnection.current = null
      conversationReads.current.invalidate()
      setNoticeText("还没确认结果，正在重新查。")
    }
    scheduleRefresh()
  }
  const prepareAttachment = async (file: AttachmentFile) => {
    const bytes = new Uint8Array(await file.arrayBuffer())
    if (!bytes.length || bytes.length > attachmentLimit) throw new Error(`附件「${file.name}」需在 64 MiB 以内且不能为空。`)
    const hash = await sha256Hex(bytes)
    return { hash, size: bytes.length, name: attachmentName(file.name), data: bytesToBase64(bytes) }
  }
  const uploadAttachment = async (id: string, file: NonNullable<OutboxItem['uploads']>[number], targetGuard?: () => void) => {
    const bytes = base64ToBytes(file.data)
    if (!bytes.length || bytes.length !== file.size || bytes.length > attachmentLimit) throw new Error('本地附件内容不完整，已保留原输入。')
    let offset = 0
    for (;;) {
      requireCurrent()
      targetGuard?.()
      const chunk = bytes.subarray(offset, offset + 1024 * 1024)
      const reply = record(await rpc("upload_attachment", { conversation_id: id, hash: file.hash, size: bytes.length, offset, data: bytesToBase64(chunk) }, targetGuard))
      if (reply.complete === true) return
      if (typeof reply.offset !== 'number' || !Number.isSafeInteger(reply.offset) || reply.offset < 0 || reply.offset > bytes.length || reply.offset === offset) throw new Error('附件上传进度无效，已保留原输入。')
      offset = reply.offset
    }
  }
  const targetConversation = (target: InputTarget, explicit?: string) => {
    const task = 'task_id' in target ? workspaceStateRef.current?.tasks.find((item) => item.task_id === target.task_id) : undefined
    const botId = 'bot_id' in target ? target.bot_id : task?.owner_bot
    const bot = botsRef.current.find((item) => item.bot_id === botId)
    const work = target.kind === 'work' ? openWorksRef.current.find((item) => item.work_id === target.work_id) ?? [...knownWorks.current.values()].find((item) => item.work_id === target.work_id) : undefined
    const pending = messagesRef.current.find((item) => inputTargetKey(item.target) === inputTargetKey(target))
    const id = bot?.conversation_id ?? work?.conversation_id ?? (target.kind === 'work' && dataRef.current?.works.some((item) => item.work_id === target.work_id) ? dataRef.current.conversation?.conversation_id : undefined) ?? pending?.conversation_id ?? explicit
    if (!id || explicit && explicit !== id || !botsRef.current.some((item) => item.conversation_id === id)) throw new Error('还没读取这个输入目标，先查询它的任务或 Bot。')
    return id
  }
  const readWork = async (input: { conversation_id: string; work_id: string }) => {
    requireCurrent()
    const detail = parseWorkDetail(await rpc('work_detail', input))
    requireCurrent()
    if (detail.work.conversation_id !== input.conversation_id || detail.work.work_id !== input.work_id) throw new Error('设备返回了其他工作的详情。')
    knownWorks.current.set(JSON.stringify([input.conversation_id, input.work_id]), detail.work)
    return detail
  }
  const messageLane = (message: Pick<PendingMessage, 'target' | 'conversation_id'>) => {
    const target = message.target
    const scope = target.kind === 'task' || target.kind === 'channel_task' ? JSON.stringify(['workspace_task', target.task_id]) : inputTargetKey(target)
    return `${message.conversation_id}:${scope}`
  }
  const requireInputEntry = (entry: string) => {
    requireCurrent()
    if (connection.info.device_id !== entry || connection.entry.device_id !== entry) throw new Error('请连接这条输入的原设备后再操作。')
  }
  const withPendingInput = async <T,>(messageId: string, action: (item: OutboxItem) => Promise<T>): Promise<T> => {
    requireCurrent()
    const read = async () => {
      const item = (await outbox.all()).find((item) => item.key === outboxKey('message', messageId))
      if (!item) throw new Error('这条输入已不在本地待发送队列。')
      return item
    }
    const lane = messageLane(parsePendingMessage(await read()))
    if (messageTargets.current.has(lane)) throw new Error('正在处理这个目标的输入，请稍后查询。')
    return withInputDelivery(lane, async () => { requireCurrent(); return action(await read()) })
  }
  // The caller holds this target's delivery lock throughout the read and reconciliation.
  const readInputReceipt = async (input: { message_id: string; target: InputTarget; conversation_id?: string }): Promise<InputReceipt | InputNotSaved> => {
    requireCurrent()
    const id = targetConversation(input.target, input.conversation_id)
    const original = (await outbox.all()).find((item) => item.key === outboxKey('message', input.message_id))
    const pending = original ? parsePendingMessage(original) : null
    const reconcile = original && pending?.conversation_id === id && inputTargetKey(pending.target) === inputTargetKey(input.target) && original.entry_device_id === connection.info.device_id && original.entry_device_id === connection.entry.device_id && (pending.delivery === 'unknown' || pending.delivery === 'submitting') && original.release_expected_event === undefined && original.release_command_id === undefined
    requireCurrent()
    if (reconcile && original.entry_device_id !== undefined) requireInputEntry(original.entry_device_id)
    const value = await rpc('input_receipt', { message_id: input.message_id, conversation_id: id, target: input.target, ...(reconcile ? { entry_device_id: original.entry_device_id, original_input: original.params } : {}) })
    requireCurrent()
    const notSaved = parseInputNotSaved(value)
    if (notSaved) {
      if (!reconcile || notSaved.entry_device_id !== original.entry_device_id || notSaved.network_id !== networkId || notSaved.client_id !== clientId || notSaved.conversation_id !== id || notSaved.message_id !== input.message_id || inputTargetKey(notSaved.target) !== inputTargetKey(input.target)) throw new Error('设备返回的输入状态与原内容不一致，原输入已保留。')
      requireInputEntry(notSaved.entry_device_id)
      const latest = (await outbox.all()).find((item) => item.key === original.key)
      if (latest?.rejection_code === 'input_conflict') return notSaved
      if (latest?.receipt !== undefined) return latest.receipt
      if (latest && latest.entry_device_id === notSaved.entry_device_id && (latest.delivery === 'unknown' || latest.delivery === 'submitting') && latest.release_expected_event === undefined && latest.release_command_id === undefined) {
        await outbox.put({ ...latest, delivery: 'saved', receipt: undefined, rejected: notSaved.reason, rejection_code: 'input_not_saved' })
        await outboxChanged()
      }
      return notSaved
    }
    const receipt = parseInputReceipt(value)
    if (receipt.message_id !== input.message_id || inputTargetKey(receipt.target) !== inputTargetKey(input.target)) throw new Error('输入回执和查询目标不匹配，已保留原输入。')
    const items = await outbox.all(), item = items.find((item) => item.key === outboxKey('message', input.message_id))
    // A receipt identifies the remote input, but cannot resolve a payload conflict.
    if (item?.rejection_code === 'input_conflict') return receipt
    if (item) {
      const releaseRejected = item.release_expected_event !== undefined && items.some((entry) => {
        if (entry.method !== 'workspace_command' || entry.command_rejection === undefined || entry.params.command_id !== item.release_command_id) return false
        const command = parseWorkspaceCommand(entry.params.command)
        return command.kind === 'control_input' && command.action.kind === 'release' && command.message_id === input.message_id && command.expected_event === item.release_expected_event && parseWorkspaceCommandNotSaved(entry.command_rejection, item.release_command_id) !== null
      })
      const releaseUnconfirmed = item.release_expected_event !== undefined && !releaseRejected && (!receipt.control_event?.trim() || receipt.control_event === item.release_expected_event)
      const uncertain = receipt.stage === 'unknown' || releaseUnconfirmed
      await outbox.put({ ...item, receipt: releaseUnconfirmed ? item.receipt ?? receipt : receipt, delivery: uncertain ? 'unknown' : 'accepted', ...(item.rejection_code === 'input_not_saved' || item.rejection_code === 'input_missing' ? { rejected: undefined, rejection_code: undefined } : {}), ...(!releaseUnconfirmed ? { release_expected_event: undefined, release_command_id: undefined } : {}) })
      await outboxChanged()
      if (!uncertain) flushMessagesRef.current()
    }
    return receipt
  }
  const queryInput = async (input: Parameters<typeof readInputReceipt>[0]) => {
    requireCurrent()
    const id = targetConversation(input.target, input.conversation_id)
    return withInputDelivery(messageLane({ target: input.target, conversation_id: id }), () => readInputReceipt({ ...input, conversation_id: id }))
  }
  const confirmInputEntry = (messageId: string, expectedEntry: string) => withPendingInput(messageId, async () => {
    requireInputEntry(expectedEntry)
    throw new Error('原输入的设备记录已经固定，不能重新指定原设备。')
  })
  const readScopeContext = async (input: { target: InputTarget; conversation_id?: string }) => {
    requireCurrent()
    const id = targetConversation(input.target, input.conversation_id)
    const next = parseScopeContext(await rpc('scope_context', { conversation_id: id, target: input.target }))
    requireCurrent()
    if (next.conversation_id !== id || inputTargetKey(next.target) !== inputTargetKey(input.target)) throw new Error('设备返回了其他目标的上下文。')
    return next
  }
  const flushMessageTarget = async (lane: string) => {
    const targets = messageTargets.current
    if (targets.has(lane) || !current() || !tabs.leader()) return
    targets.add(lane)
    const deliver = async () => {
      for (;;) {
        if (!current()) return
        const item = (await outbox.all()).filter((item): item is OutboxRecord<'message'> => item.method === 'message' && item.delivery !== 'accepted' && messageLane(parsePendingMessage(item)) === lane).sort((a, b) => Number(a.params.sequence) - Number(b.params.sequence) || a.created_at_ms - b.created_at_ms)[0]
        if (!item || item.rejected !== undefined || item.delivery === 'saved' && item.rejection_code === 'input_not_saved') return
        requireMessageTarget(item)
        const pending = parsePendingMessage(item)
        if (item.delivery === 'unknown' || item.delivery === 'submitting') {
          await readInputReceipt(pending)
          const checked = (await outbox.all()).find((entry) => entry.key === item.key)
          if (checked?.delivery !== 'accepted') return
          continue
        }
        let sending = item
        try {
          if (item.entry_device_id !== undefined) requireInputEntry(item.entry_device_id)
          if (item.prerequisite) {
            const command = parseWorkspaceCommand(item.prerequisite.params.command)
            const known = command.kind === 'create_task' && workspaceStateRef.current?.tasks.some((task) => task.task_id === command.task_id && task.owner_bot === command.owner_bot && task.channel_id === command.channel_id)
            if (!known) await submitWorkspaceCommand(item.prerequisite, () => requireMessageTarget(sending))
            requireMessageTarget(sending)
            const ready = { ...sending, prerequisite: undefined }
            requireMessageTarget(ready)
            sending = ready
          }
          if (item.uploads?.length) {
            sending = { ...sending, delivery: 'uploading' }
            await outbox.put(sending); await outboxChanged()
            for (const file of item.uploads) await uploadAttachment(pending.conversation_id, file, () => requireMessageTarget(sending))
          }
          requireCurrent()
          if (!(await outbox.all()).some((entry) => entry.key === item.key)) continue
          const entry = item.entry_device_id ?? connection.info.device_id
          requireInputEntry(entry)
          requireMessageTarget(sending)
          sending = { ...sending, delivery: 'submitting', entry_device_id: entry }
          await outbox.put(sending); await outboxChanged()
          requireInputEntry(entry)
          const receipt = await rpc('message', item.params)
          requireCurrent()
          const result = record(receipt)
          if (result.accepted === false && result.message_id === pending.message_id && typeof result.reason === 'string') {
            const notSaved = result.code === 'input_not_saved' && result.stored === false
            const conflict = result.code === 'input_conflict' && result.stored === true
            if (notSaved || conflict) {
              if ((await outbox.all()).some((entry) => entry.key === item.key)) await outbox.put({ ...sending, delivery: notSaved ? 'saved' : 'unknown', rejected: result.reason, rejection_code: notSaved ? 'input_not_saved' : 'input_conflict' })
              await outboxChanged()
              showError(new Error(result.reason))
              return
            }
          }
          if (!isQueuedReceipt(receipt, pending)) throw new Error('接收设备还没确认收到这条输入。请查询原消息。')
          if ((await outbox.all()).some((entry) => entry.key === item.key)) await outbox.put({ ...sending, delivery: 'accepted', uploads: undefined, receipt: { message_id: pending.message_id, target: pending.target, stage: 'entry_accepted', actual_turn_id: null, reason: null, retry_safe: false, control_event: null } })
          await outboxChanged()
          scheduleRefresh()
        } catch (caught) {
          if (renderedConnection.current !== connection) return
          if (caught instanceof UnconfirmedInputTargetError) { showError(caught); return }
          const submitted = sending.delivery === 'submitting'
          if ((await outbox.all()).some((entry) => entry.key === item.key)) await outbox.put({ ...sending, delivery: submitted ? 'unknown' : 'saved', ...(!submitted && responded(caught) ? { rejected: caught instanceof Error ? caught.message : String(caught) } : {}) })
          await outboxChanged()
          showError(caught, submitted ? '确认输入是否送达' : '发送已保存的输入')
          return
        }
      }
    }
    try { await withInputDelivery(lane, deliver) }
    catch (caught) { showError(caught, '查询已保存的输入') }
    finally { targets.delete(lane) }
  }
  const flushMessages = () => {
    if (!current() || !tabs.leader()) return
    void outbox.all().then((items) => {
      const lanes = new Set(items.filter((item) => item.method === 'message' && item.delivery !== 'accepted').map((item) => messageLane(parsePendingMessage(item))))
      for (const lane of lanes) void flushMessageTarget(lane)
    }).catch((caught) => showError(caught, '读取已保存的输入'))
  }
  flushMessagesRef.current = flushMessages
  const canSaveMessages = !ledgerFailed && joined && booted && loadedMessageConnection.current === connection && renderedConnection.current === connection && writeState.state === 'writable' && bots.length > 0
  const sendMessage = async (text: string, files: AttachmentFile[] = [], sent: Attachment[] = [], options?: InputOptions) => {
    ledgerGuard()
    if (!canSaveMessages || !alive.current) {
      await outbox.all(); ledgerGuard()
      throw new Error(writeState.state === 'writable' ? '先读取已授权的工作区，再保存输入。' : writeNotices[writeState.state])
    }
    const selected = bots.find((bot) => bot.conversation_id === activeId.current)
    let target: InputTarget = options?.target ?? (selected ? { kind: 'bot_main', bot_id: selected.bot_id } : (() => { throw new Error('还没选 Bot。先选一个。') })())
    const id = targetConversation(target, options?.conversation_id)
    let revision = options?.expected_revision
    let localTask: LocalTask | undefined
    const intent = options?.intent ?? 'continue'
    if (intent === 'new_task' || target.kind === 'channel_member') {
      if (!files.length && !sent.length) inputText(text, '消息内容', 65536)
      const bot = botsRef.current.find((bot) => bot.conversation_id === id)
      if (!bot) throw new Error('输入目标没有可用的 Bot。')
      const channelKey = target.kind === 'channel_member' ? inputTargetKey(target) : undefined
      localTask = channelKey && intent !== 'new_task' ? channelTasks.current.get(channelKey) : undefined
      if (!localTask) {
        const taskId = newId(), channelId = 'channel_id' in target ? target.channel_id : undefined
        const sources = target.kind === 'work' ? knownWorks.current.get(JSON.stringify([id, target.work_id]))?.source_events ?? [] : []
        const command = inputTaskCommand({ task_id: taskId, channel_id: channelId, owner_bot: bot.bot_id, text, source_events: sources })
        localTask = { target: channelId ? { kind: 'channel_task', channel_id: channelId, task_id: taskId, bot_id: bot.bot_id } : { kind: 'task', task_id: taskId }, conversation_id: id, command, saved: false }
        if (channelKey) channelTasks.current.set(channelKey, localTask)
      }
      target = localTask.target
      revision = localTask.saved ? options?.expected_revision : 1
    }
    const lane = messageLane({ target, conversation_id: id })
    const saves = messageSaves.current
    const previous = saves.get(lane) ?? Promise.resolve()
    localMessageSaves.current += 1; setMessageBusy(true)
    const save = inputLock(outboxKey('input-save', lane), () => previous.catch(() => {}).then(async () => {
      await outbox.all(); ledgerGuard()
      const uploads = await Promise.all(files.map(prepareAttachment))
      ledgerGuard()
      if (renderedConnection.current !== connection || !alive.current) throw new Error('工作区已切换，输入仍留在编辑器。')
      let prerequisite: OutboxRecord<'workspace_command'> | undefined
      if (target.kind === 'task' || target.kind === 'channel_task') {
        const key = inputTargetKey(target)
        prerequisite = (await outbox.all()).find((item) => item.method === 'message' && item.prerequisite && inputTargetKey(parsePendingMessage(item).target) === key)?.prerequisite
      }
      if (!prerequisite && localTask?.command) {
        if (!localTask.saved) localTask.command = inputTaskCommand({ ...localTask.command, text })
        prerequisite = await queueWorkspaceCommand(localTask.command, false)
      }
      if ('task_id' in target && revision === undefined) {
        const taskId = target.task_id
        const key = inputTargetKey(target)
        revision = workspaceStateRef.current?.tasks.find((task) => task.task_id === taskId)?.revision
          ?? [...messagesRef.current].reverse().find((item) => inputTargetKey(item.target) === key)?.expected_revision
      }
      if ('task_id' in target && revision === undefined) throw new Error('任务版本尚未读取，先查询任务。')
      const attachments = signedOrder([...sent, ...uploads.map(({ data: _, ...file }) => file)])
      const sequenceKey = scoped('input-sequences')
      const prepared = await inputLock(sequenceKey, async () => {
        ledgerGuard()
        let sequences: Record<string, number>
        try { sequences = inputSequences(readStorage(sequenceKey, {}, true)) } catch { return corruptLedger() }
        const prior = sequences[lane]
        if (prior !== undefined && (typeof prior !== 'number' || !Number.isSafeInteger(prior) || prior < 0)) throw new Error('本地输入顺序损坏，已暂停发送。')
        if (prior === Number.MAX_SAFE_INTEGER) return corruptLedger()
        const sequence = (prior ?? 0) + 1
        const prepared = prepareSend({ conversationId: id, text, target, sequence, intent, expected_revision: revision, reportResults: true, files: files.length + sent.length > 0 })
        if (!prepared.ok) throw new Error(prepared.error)
        writeStorage(sequenceKey, { ...sequences, [lane]: sequence }, true)
        return prepared
      })
      const body = { ...prepared.body, ...(attachments.length ? { attachments } : {}) }
      const attestation = await attest(id, writeBody({ method: "message", params: body }, clientId))
      if (renderedConnection.current !== connection || !alive.current) throw new Error('工作区已切换，输入仍留在编辑器。')
      const queued = await enqueue(outbox, { key: prefix + writeKeySuffix({ method: 'message', params: body }), method: 'message', scope_id: id, params: { ...body, attestation }, created_at_ms: prepared.pending.created_at_ms, uploads, delivery: 'saved', ...(prerequisite ? { prerequisite } : {}) })
      if (localTask) localTask.saved = true
      await outboxChanged().catch((caught) => showError(caught, '读取已保存的输入'))
      try { options?.onSaved?.(parsePendingMessage(queued)) }
      catch (caught) { showError(caught, '更新已保存输入的目标') }
      flushMessages()
    }))
    saves.set(lane, save)
    try { await save; return true }
    finally {
      if (saves.get(lane) === save) saves.delete(lane)
      localMessageSaves.current = Math.max(0, localMessageSaves.current - 1)
      if (renderedConnection.current === connection && alive.current) setMessageBusy(localMessageSaves.current > 0)
    }
  }
  const retryPendingMessage = (messageId: string) => withPendingInput(messageId, async (item) => {
    if (renderedConnection.current !== connection || !alive.current) throw new Error('工作区已切换，先读取当前输入。')
    const pending = parsePendingMessage(item)
    const lane = messageLane(pending), targets = messageTargets.current
    if (targets.has(lane)) throw new Error('正在处理这个目标的输入，请稍后查询。')
    targets.add(lane)
    let resume = false
    try {
      if (pending.rejection_code === 'input_missing') throw new Error(inputMissingReason)
      if (pending.rejection_code === 'input_conflict') throw new Error('设备保存的原输入和本地内容不同，已保留两边内容。请先查看原输入。')
      const queued = workspaceStateRef.current?.inputs.find((input) => input.input.message_id === messageId && input.conversation_id === pending.conversation_id && inputTargetKey(input.input.target) === inputTargetKey(pending.target))
      if (queued?.cancelled || queued?.deferred) throw new Error('这条输入已保存到设备。请查看状态，再使用对应操作。')
      if (pending.delivery === 'saved' && pending.receipt === undefined) {
        requireWritable()
        if (pending.rejection_code === 'input_not_saved' && item.entry_device_id === undefined) throw new Error('这条输入缺少原设备记录，已保留原内容。')
        if (item.entry_device_id !== undefined) requireInputEntry(item.entry_device_id)
        await outbox.put({ ...item, rejected: undefined, rejection_code: undefined, receipt: undefined, delivery: 'saved' })
        await outboxChanged()
        resume = true
        return
      }
      const receipt = await readInputReceipt(pending)
      const latest = (await outbox.all()).find((entry) => entry.key === item.key)
      if (!latest) { resume = true; return }
      resume = latest.delivery === 'accepted'
      if (pending.delivery !== 'accepted' || item.release_expected_event !== undefined || !('stage' in receipt) || receipt.stage !== 'refused' || !receipt.retry_safe || !receipt.control_event?.trim()) {
        setNoticeText(receipt.reason ?? '已保留原输入，未重复提交。')
        return
      }
      if (latest.rejection_code === 'input_conflict') throw new Error('设备保存的原输入和本地内容不同，已保留两边内容。请先查看原输入。')
      requireWritable()
      const control = await queueWorkspaceCommand({ kind: 'control_input', message_id: messageId, expected_event: receipt.control_event, action: { kind: 'release' } })
      const commandId = control.params.command_id
      if (typeof commandId !== 'string') throw new Error('本地操作缺少编号，原输入已保留。请再查询。')
      if (control.command_rejection !== undefined) await outbox.put({ ...control, command_rejection: undefined, rejected: undefined })
      await outbox.put({ ...latest, receipt, delivery: 'unknown', release_expected_event: receipt.control_event, release_command_id: commandId })
      await outboxChanged()
      resume = false
      let failure: unknown
      try { await submitWorkspaceCommand(control); scheduleRefresh() }
      catch (caught) { failure = caught }
      try { await readInputReceipt(pending) }
      catch (caught) { throw failure ?? caught }
      const confirmed = (await outbox.all()).find((entry) => entry.key === item.key)
      resume = confirmed?.delivery === 'accepted'
      if (failure instanceof WorkspaceCommandNotSavedError) throw failure
      if (confirmed?.release_expected_event !== undefined) {
        if (failure !== undefined) throw failure
        setNoticeText('重试结果尚未确认，原输入已保留。请再查询。')
      }
    } finally {
      targets.delete(lane)
      if (resume) flushMessages()
    }
  })
  const releaseMessagePending = (messageId: string) => withPendingInput(messageId, async (item) => {
    if (renderedConnection.current !== connection || !alive.current) throw new Error('工作区已切换，先读取当前输入。')
    const pending = parsePendingMessage(item)
    if (messageTargets.current.has(messageLane(pending))) throw new Error('这个目标仍在提交输入，先查询原输入的结果。')
    if (pending.rejection_code === 'input_missing') throw new Error(inputMissingReason)
    if (pending.rejection_code === 'input_conflict') throw new Error('设备保存的原输入和本地内容不同，已保留两边内容。请先查看原输入。')
    let notice = '已从本机待发送队列移除。'
    if (pending.delivery === 'unknown' || pending.delivery === 'submitting' || pending.delivery === 'accepted' || pending.receipt !== undefined || pending.release_expected_event !== undefined || pending.release_command_id !== undefined) {
      const receipt = await readInputReceipt(pending)
      const latest = (await outbox.all()).find((entry) => entry.key === item.key)
      if (!latest) return
      const checked = parsePendingMessage(latest)
      if (checked.message_id !== pending.message_id || checked.conversation_id !== pending.conversation_id || inputTargetKey(checked.target) !== inputTargetKey(pending.target)) throw new Error('本地输入已变化，内容仍保留。请再查询。')
      if (checked.rejection_code === 'input_conflict' || !('stage' in receipt) || checked.delivery !== 'accepted' || receipt.stage !== 'refused' || checked.receipt?.stage !== 'refused' || checked.release_expected_event !== undefined || checked.release_command_id !== undefined) throw new Error('这条输入还不能关闭提醒，原内容已保留。请继续查询结果。')
      notice = '已关闭这条输入的提醒。'
    }
    requireCurrent()
    await outbox.delete(item.key)
    await outboxChanged()
    flushMessages()
    setNoticeText(notice)
  })
  const interruptRegisteredWork = async (id: string, workId: string) => {
    requireCurrent()
    const work = (dataRef.current?.conversation?.conversation_id === id ? dataRef.current.works.find((item) => item.work_id === workId) : undefined)
      ?? openWorksRef.current.find((item) => item.conversation_id === id && item.work_id === workId)
      ?? knownWorks.current.get(JSON.stringify([id, workId]))
    if (!work || work.outcome || !['accepted', 'running'].includes(work.state)) throw new Error('这个工作现在停不了。再查一次。')
    let receipt: Record<string, unknown>
    try { receipt = record(await rpc('interrupt_work', { conversation_id: id, work_id: workId })) }
    catch (caught) { requireControlRefresh(caught); throw caught }
    requireCurrent()
    setNoticeText(receipt.waiting_for_device === true ? '已请求停止这个工作，等待设备确认。' : '已请求停止这个工作。')
    scheduleRefresh()
  }
  const stopInputTarget = (options: { target: InputTarget; expected_revision?: number }) => withMutation(`stop-target:${inputTargetKey(options.target)}`, async () => {
    const target = options.target
    if (target.kind === 'task' || target.kind === 'channel_task') {
      const state = await readWorkspaceState({ task_id: target.task_id })
      const task = state.tasks.find((item) => item.task_id === target.task_id)
      if (!task) throw new Error('这个任务没有读取到，先查询任务。')
      const affected = state.assignments.filter((item) => item.task_id === task.task_id && item.state !== 'completed' && item.state !== 'stopped')
      await workspaceCommand({ kind: 'control_task', task_id: task.task_id, expected_revision: options.expected_revision ?? task.revision, action: 'stop' })
      setNoticeText(`已记录停止请求，涉及 ${affected.length} 项执行${affected.some((item) => item.state === 'unknown') ? '，部分结果尚未确认' : ''}。仍需运行设备确认。`)
      return
    }
    const id = targetConversation(target)
    if (target.kind === 'work') {
      await interruptRegisteredWork(id, target.work_id)
      return
    }
    if (target.kind === 'channel_member') throw new Error('先选择频道中的具体任务，再请求停止。')
    const context = parseConversation(await rpc('conversation', { conversation_id: id }), id)
    const triggers = target.kind === 'routine' ? [target.trigger] : context.scheduler?.jobs.filter((job) => !isTerminalJobState(job.state) && job.scope.kind === 'main').map((job) => job.trigger) ?? []
    if (!triggers.length) throw new Error('这个目标当前没有可停止的回复。')
    const receipts = await Promise.all(triggers.map((trigger) => rpc('cancel_manager', { conversation_id: id, trigger }).then(record)))
    requireCurrent()
    const unknown = receipts.some((receipt) => receipt.unconfirmed === true)
    setNoticeText(`已请求停止 ${triggers.length} 项回复${unknown ? '，仍有请求未确认' : ''}。已派发的工作需单独停止。`)
    scheduleRefresh()
  })
  const sameProject = (left?: BotProject, right?: BotProject) => left?.target_node === right?.target_node && left?.cwd === right?.cwd
  const clearPendingBot = () => {
    writeStorage(scoped("pending-bot"), null, true)
    botCreationRef.current = null; setPendingBot(null)
  }
  const checkRejectedBot = async (pending: PendingBot): Promise<BotRecord | null> => {
    if (pending.submission?.state !== "rejected" || pending.submission.entry_node !== connection.entry.device_id) throw new Error("内容已保留。回到原来的设备确认 Bot 是否建好。")
    const known = parseBots(await rpc("bots"))
    requireCurrent()
    const match = known.find((bot) => bot.bot_id === pending.bot_id || bot.conversation_id === pending.conversation_id)
    if (match && (match.bot_id !== pending.bot_id || match.conversation_id !== pending.conversation_id || match.name !== pending.name || match.instructions !== pending.instructions || !sameProject(match.project, pending.project))) throw new Error("找到了这个 Bot。先确认它是否已经建好。")
    clearPendingBot()
    if (!match) setNoticeText("确认没建成，可以修改后再试。")
    return match ?? null
  }
  const selectedBot = bots.find((bot) => bot.conversation_id === conversationId) ?? null
  const selectBot = async (bot: BotRecord) => {
    requireCurrent()
    writeStorage(scoped("selected-bot"), bot.bot_id)
    selectConversation(bot.conversation_id)
    if (conversationCache) {
      scheduleRefresh()
      await readConversation(bot.conversation_id)
    } else await refreshRef.current()
  }
  /**
   * 重试 for the newest reply, which failed or was stopped. The conversation is read again first, so a reply that went on since
   * is not repeated; a reply that may have done part of its work is followed by a message asking the bot to check before redoing.
   */
  const retryReply = async (messageId: string) => {
    const id = activeId.current
    await refreshRef.current()
    requireCurrent()
    const shown = dataRef.current, messages = shown?.messages ?? []
    const asked = messages.find((message) => message.message_id === messageId && message.role === "user")
    const newest = messages.at(-1)
    const job = shown?.scheduler?.jobs.find((item) => item.message_id === messageId)
    if (activeId.current !== id || !asked || (newest?.message_id !== messageId && newest?.in_reply_to !== messageId) || !job || !["failed", "rejected", "cancelled", "interrupted"].includes(job.state) || outcomeUnknown(job.failure?.kind, job.failure?.detail)) {
      setNoticeText("这条已经有新进展了，没有重发。")
      return false
    }
    const traces = (shown?.works ?? []).some((work) => work.trigger === job.trigger) || (shown?.changes ?? []).some((change) => change.cause?.message_id === messageId)
    return sendMessage(retryText(asked.text, job.failure?.kind, traces), [], asked.attachments ?? [])
  }
  const visibleData = dataScope.current.networkId === networkId && dataScope.current.clientId === clientId ? data : null
  const visibleOrigin: typeof dataOrigin = visibleData ? dataOrigin : 'none'
  const incomplete = !visibleData
  // The lists hold this connection's first full read and the Node is not aligning with other devices; until then an empty list means not yet known.
  const synced = sameRead() && writeState.state !== "aligning"
  const chatSlot = { conversationId: conversationId || null }

  return {
    info: connection.info, generation: connectionGeneration, isCurrent: current, connectionOnline: current(), connectionText: current() ? "已建立端到端加密连接" : "连接已断开，输入可保存在本机；结果未知的输入只查询，不自动重发。",
    error, noticeText, view, setView: (next: WorkspaceView) => {
      viewRef.current = next; setView(next); clearMessages()
      if (next === "chat" || next === "tasks") scheduleRefresh()
      else { onScreen.current = null; setFresh(new Set<string>()) }
    }, nodes, nodesRead: nodeReport !== null, nodeReport, refreshNodes, conversationId, data: visibleData, dataOrigin: visibleOrigin, conversationLoading, conversationError, refreshConversation, markMessagesRead, fresh, notifications, ignoredNotifications, questions, openWorks, overview, answered, pendingAnswers, bots, primary, selectedBot, pendingBot,
    historyLoading, loadHistory, cancelHistory, refreshing, incomplete, chatSlot, pendingMessages, messageBusy, canSaveMessages, liveProgress, interrupted,
    workspaceState, readWorkspaceState, readWorkspaceHistory, workspaceCommand, queryInput, confirmInputEntry, readScopeContext, readWork, stopInputTarget, retryPendingMessage, releaseMessagePending,
    controlInput: (input: { message_id: string; expected_event: string; action: InputControlAction }) => workspaceCommand({ kind: 'control_input', ...input }),
    joined, synced,
    writable: !ledgerFailed && joined && booted && current() && sameRead() && writeState.state === "writable",
    writeNotice: ledgerFailed ? outboxCorruptNotice : !sameRead() ? "正在补读这个节点的记录，已暂停写入。" : "",
    writeState, readOnlyNotice: writeState.state === "writable" ? "" : writeNotices[writeState.state],
    diagnostics: (sinceMs: number) => { requireCurrent(); return connection.request("diagnostics", { since_ms: sinceMs }) },
    logs: async (filter: LogFilter) => { requireCurrent(); return parseLogs(await connection.request("logs", filter)) },
    confirmWritable: async () => { await outbox.all(); ledgerGuard(); setWriteState(parseWriteStatus(await connection.request("confirm_writable"))); scheduleRefresh() },
    syncText: visibleOrigin === 'cache' ? '已显示本机保存的聊天记录，等待补读。' : !visibleData ? "发一条消息开始对话。" : "已加载这个节点上的记录。",
    refresh, run, announce: (notice: string) => { if (current()) setNoticeText(notice) }, notificationButton, ignoreNotifications, markNotificationsRead,
    ackNotification,
    selectBot,
    createBot: (input: { name: string; instructions: string; project?: BotProject }) => withMutation("create-bot", async () => {
      const name = inputText(input.name, "Bot 名称", 512), instructions = input.instructions.trim()
      if (new TextEncoder().encode(instructions).length > 8192) throw new Error("职责说明不能超过 8192 字节。")
      if (new TextEncoder().encode(JSON.stringify([name, instructions, input.project ?? null])).length > 12000) throw new Error("内容太长了。缩短名称或说明。")
      const prior = botCreationRef.current
      if (prior && (prior.name !== name || prior.instructions !== instructions || !sameProject(prior.project, input.project))) throw new Error("Bot 还没确认建好，内容已保留。原样重试。")
      if (prior?.submission?.state === "rejected") {
        const known = await checkRejectedBot(prior)
        if (known) { await selectBot(known); return known }
        throw new Error("确认没建成，可以修改后再试。")
      }
      const pending: PendingBot = prior ?? { bot_id: newId(), conversation_id: newId(), name, instructions, project: input.project, submission: { state: "unconfirmed", entry_node: connection.entry.device_id } }
      const request: CreateBotRequest = parseCreateBot(pending)
      writeStorage(scoped("pending-bot"), pending, true)
      botCreationRef.current = pending; setPendingBot(pending)
      let response: unknown
      const attestation = await attest(request.conversation_id, writeBody({ method: "create_bot", params: { ...request, scheduler_node: pending.submission?.entry_node ?? connection.entry.device_id, owner_attestation: { device_id: clientId, signature: "" } } }, clientId))
      // The entry first asked schedules the conversation, so a retry through any entry writes the same owner.
      const scheduler = pending.submission?.entry_node ?? connection.entry.device_id
      const owner_attestation = await attest(request.conversation_id, ownerBody(scheduler, 1))
      try { response = await signedWrite(request.conversation_id, { method: "create_bot", params: { ...request, scheduler_node: scheduler, owner_attestation, attestation } }) }
      catch (caught) {
        if (!prior && current() && caught instanceof Error && "responded" in caught && caught.responded === true) {
          const rejected: PendingBot = { ...pending, submission: { state: "rejected", entry_node: connection.entry.device_id } }
          writeStorage(scoped("pending-bot"), rejected, true)
          botCreationRef.current = rejected; setPendingBot(rejected)
          const known = await checkRejectedBot(rejected)
          if (known) { await selectBot(known); return known }
          throw new Error(`${messageOf(caught)} 确认没建成，可以修改后再试。`)
        }
        throw caught
      }
      const bot = parseBot(record(response).bot)
      requireCurrent()
      if (bot.bot_id !== request.bot_id || bot.conversation_id !== request.conversation_id || bot.name !== request.name || bot.instructions !== request.instructions || !sameProject(bot.project, request.project)) throw new Error("Bot 创建结果和填写的内容不一致。检查后再试。")
      clearPendingBot()
      await selectBot(bot)
      return bot
    }),
    sendMessage, retryReply,
    cancelManager: (trigger: string) => withMutation(`cancel-manager:${trigger}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      const receipt = record(await rpc("cancel_manager", { conversation_id: id, trigger }))
      requireCurrent()
      // The tasks it started go on, so the notice says how many still run; they show in the chat.
      const stopped = (state: unknown) => {
        const running = (dataRef.current?.works ?? []).filter((work) => taskActive(work.state)).length
        return `${state === "cancelled" ? "已停止回复" : "正在停止回复"}${running ? `，还有 ${running} 项任务在跑。` : "。"}`
      }
      if (receipt.unconfirmed === true) {
        // The owner did not answer in time; the stop is not sent again, the owner's state settles it.
        if (activeId.current === id) await refreshRef.current()
        const job = dataRef.current?.scheduler?.jobs.find((item) => item.trigger === trigger)
        setNoticeText(job && ["cancelling", "cancelled"].includes(job.state) ? stopped(job.state) : "停止未确认")
        return job?.state === "cancelled"
      }
      const job = record(receipt.job)
      if (activeId.current === id) await refreshRef.current()
      if (receipt.publication_pending === true) setNoticeText("回复已经写完，正在发出来，停不了了。")
      else if (receipt.cancel_requested === true) setNoticeText(stopped(job.state))
      else setNoticeText("这次回复现在停不了。")
      return job.state === "cancelled" && receipt.publication_pending !== true
    }),
    /** Replying or declining is keyed by message, so an unknown outcome is settled by reading the conversation again. */
    settleUnanswered: (messageIds: string[], choice: "reply" | "dismiss") => withMutation(`unanswered:${activeId.current}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      try { await rpc(`${choice}_unanswered`, { conversation_id: id, message_ids: messageIds }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    makePrimary: (id: string) => withMutation(`primary:${id}`, async () => {
      const attestation = await attest("bots", writeBody({ method: "set_primary_bot", params: { conversation_id: id } }, clientId))
      try { await signedWrite("bots", { method: "set_primary_bot", params: { conversation_id: id, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      await refreshRef.current()
    }),
    /** `null` clears it; the bot then asks where to work. */
    setBotProject: (id: string, project: BotProject | null) => withMutation(`bot-project:${id}`, async () => {
      const attestation = await attest(id, writeBody({ method: "set_bot_project", params: { conversation_id: id, project } }, clientId))
      try { await signedWrite(id, { method: "set_bot_project", params: { conversation_id: id, project, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      await refreshRef.current()
    }),
    /**
     * `version` names the face the user saw and `avatar` is the new one, `null` being the face the bot's id gives. One
     * write per bot waits in the outbox, and a new pick replaces one whose outcome is unknown: the Node takes at most one
     * write signed over a version, so the two cannot both land. The bot the Node returns joins the list at once.
     */
    setBotAvatar: (id: string, version: string | null, avatar: Avatar | null) => withMutation(`bot-avatar:${id}`, async () => {
      await outbox.delete(outboxKey("bot-avatar", id))
      const attestation = await attest(id, writeBody({ method: "set_bot_avatar", params: { conversation_id: id, version, avatar } }, clientId))
      let result: unknown
      try { result = await signedWrite(id, { method: "set_bot_avatar", params: { conversation_id: id, version, avatar, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      const written = parseBot(record(result).bot)
      setBots((previous) => previous.map((bot) => bot.conversation_id === id ? { ...bot, avatar: written.avatar, avatar_version: written.avatar_version } : bot))
    }),
    controlMemory: (conversationId: string, memoryId: string, base: MemoryBase, after: string | null) => withMutation(`memory:${conversationId}:${memoryId}`, async (): Promise<MemoryControlResult> => {
      requireBotContext(conversationId)
      let result: MemoryControlResult
      try { result = await memory.controlMemory(conversationId, memoryId, base, after) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      finally { await outboxChanged() }
      requireCurrent()
      setNoticeText(memoryResultLabel(result))
      if (activeId.current === conversationId) await refreshRef.current()
      return result
    }),
    deleteBot: (id: string) => withMutation(`bot-delete:${id}`, async () => {
      const attestation = await attest(id, writeBody({ method: "delete_bot", params: { conversation_id: id } }, clientId))
      try { await signedWrite(id, { method: "delete_bot", params: { conversation_id: id, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      await reloadScope(id)
    }),
    controlSchedule: (scheduleId: string, action: ScheduleAction) => withMutation(`schedule:${scheduleId}`, async () => {
      const id = activeId.current
      const attestation = await attest(id, writeBody({ method: "control_schedule", params: { conversation_id: id, schedule_id: scheduleId, action } }, clientId))
      try { await signedWrite(id, { method: "control_schedule", params: { conversation_id: id, schedule_id: scheduleId, action, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    takeOver: () => withMutation(`owner:${activeId.current}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      if (!context.conversation) throw new Error("这个节点上还没有这个对话，可能还在同步。")
      const scheduler = context.local_node, epoch = context.conversation.owner_epoch + 1
      const attestation = await attest(id, writeBody({ method: "set_owner", params: { conversation_id: id, scheduler_node: scheduler, epoch } }, clientId))
      try { await signedWrite(id, { method: "set_owner", params: { conversation_id: id, scheduler_node: scheduler, epoch, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      setNoticeText(`已换到 ${nodes.find((node) => node.device_id === scheduler)?.name || "另一个节点"}`)
      if (activeId.current === id) await refreshRef.current()
    }),
    /** Closes a failed task in 待处理 on every device; a retry of the same close writes nothing more. */
    dismissWork: (conversationId: string, workId: string) => withMutation(`dismiss:${workId}`, async () => {
      const attestation = await attest(conversationId, writeBody({ method: "dismiss_work", params: { conversation_id: conversationId, work_id: workId } }, clientId))
      await signedWrite(conversationId, { method: "dismiss_work", params: { conversation_id: conversationId, work_id: workId, attestation } })
      requireCurrent()
      openWorksRef.current = openWorksRef.current.filter((item) => item.work_id !== workId)
      setOpenWorks(openWorksRef.current)
      if (viewRef.current === "tasks") scheduleRefresh()
    }),
    interruptWork: (workId: string) => withMutation(`work-control:${workId}`, () => interruptRegisteredWork(activeId.current, workId)),
    /**
     * The person's answer to a question: a message whose ID comes from the question, so any retry is the same write.
     * An unknown outcome stays in the outbox; trying again sends the stored request without signing anew.
     */
    answer: (question: Question, input: string) => withMutation(`answer:${question.question_id}`, async (): Promise<AnswerResult> => {
      const text = inputText(input, "回答", 65536)
      const key = prefix + writeKeySuffix({ method: "answer", params: { question_id: question.question_id, text } })
      let item = (await outbox.all()).find((entry) => entry.key === key)
      if (item?.rejected !== undefined) { await outbox.delete(key); item = undefined }
      if (item && (item.method !== "answer" || item.params.text !== text)) throw new Error("上一个回答还没确认送达，先再试一次。")
      item ??= await enqueue(outbox, {
        key, method: "answer", scope_id: question.conversation_id, created_at_ms: Date.now(),
        params: { question_id: question.question_id, text, attestation: await attest(question.conversation_id, writeBody({ method: "answer", params: { question_id: question.question_id, text } }, clientId)) },
      })
      await outboxChanged()
      let result: AnswerResult
      try { result = parseAnswer(await rpc("answer", item.params), question.question_id) }
      catch (caught) {
        if (responded(caught)) { await outbox.delete(key); await outboxChanged() }
        throw caught
      }
      await outbox.delete(key)
      await outboxChanged()
      setAnswered((previous) => ({ ...previous, [question.question_id]: result.text }))
      scheduleRefresh()
      return result
    }),
    /** A work waiting for the person, from any conversation: their signed choice settles it. */
    resolveCheck: (check: Extract<PendingCheck, { kind: "work" }>, choice: "completed" | "interrupted", note: string) => withMutation(`pending-check:${check.check_id}`, async () => {
      const id = check.conversation_id
      if (check.check_id !== `work:${check.work_id}`) throw new Error("工作确认目标不一致，请重新读取。")
      const text = note.trim()
      if (new TextEncoder().encode(text).length > 4096) throw new Error("备注太长了。")
      const attestation = await attest(id, writeBody({ method: "resolve", params: { conversation_id: id, check_id: check.check_id, outcome: choice, note: text } }, clientId))
      try { await signedWrite(id, { method: "resolve", params: { conversation_id: id, check_id: check.check_id, outcome: choice, note: text, attestation } }) }
      catch (caught) { requireControlRefresh(caught); throw caught }
      requireCurrent()
      await refreshRef.current()
    }),
    /** A coordination process still alive: the person confirms it has stopped. */
    confirmStopped: (check: Extract<PendingCheck, { kind: "process" }>) => withMutation(`pending-check:${check.check_id}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      await rpc("resolve", { conversation_id: id, check_id: check.check_id })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    readArtifactBytes: async (hash: string, offset = 0, conversationId = activeId.current) => {
      requireCurrent()
      const result = parseBinaryPage(await rpc("read_artifact", { conversation_id: conversationId, blob_hash: hash, offset, binary: true }), hash, offset)
      requireCurrent()
      return result
    },
    readArtifact: async (hash: string, offset = 0, conversationId?: string) => {
      const id = conversationId ?? activeId.current
      requireCurrent()
      const result = parseArtifactPage(await rpc("read_artifact", { conversation_id: id, blob_hash: hash, offset, limit: 65536 }), hash, offset)
      requireCurrent()
      if (conversationId === undefined && activeId.current !== id) throw new Error("对话已切换。回到原对话重新打开产物。")
      return result
    },
    enableNotifications: async () => {
      requireCurrent()
      const granted = await notifier.permission()
      if (granted === "unsupported") return
      const permission = granted === "granted" ? granted : await notifier.request()
      requireCurrent()
      const preferences = notificationPreferences(platform.storage, networkId)
      preferences.write({ ...preferences.read(), enabled: permission === "granted" })
      if (permission === "granted") { writeStorage(networkKey("seen", networkId), notificationsRef.current.map((item) => item.notification_id)); setNoticeText(`${notifier.name}已开启。已有未读提醒保留在对话列表中。`) }
      await refreshNotificationButton()
    },
  }
}

const WorkspaceContext = createContext<ReturnType<typeof useWorkspaceState> | null>(null)
export function useWorkspace() {
  const value = useContext(WorkspaceContext)
  if (!value) throw new Error("工作区组件缺少上下文。")
  return value
}
export function WorkspaceProvider({ platform, connection, openConversation, tabs, initialBot = null, children }: { platform: Platform; connection: ReadyConnection; openConversation: (conversationId: string, target?: InputTarget) => void; tabs: Tabs; initialBot?: string | null; children: ReactNode }) {
  const value = useWorkspaceState(platform, connection, openConversation, tabs, initialBot)
  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>
}

function stringList(value: unknown): string[] | null {
  return Array.isArray(value) && value.every((item): item is string => typeof item === 'string') ? value : null
}
function interruptedReplies(value: unknown): InterruptedReply[] {
  if (!Array.isArray(value)) return []
  try { return value.map(value => { const data = record(value); if (typeof data.trigger !== 'string' || typeof data.conversation_id !== 'string' || typeof data.text !== 'string' || data.after !== undefined && typeof data.after !== 'string') throw new Error(); return { trigger: data.trigger, conversation_id: data.conversation_id, text: data.text, ...(data.after === undefined ? {} : { after: data.after }) } }) } catch { return [] }
}
function inputSequences(value: unknown): Record<string, number> {
  const data = record(value), result: Record<string, number> = Object.create(null)
  for (const [key, value] of Object.entries(data)) {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new OutboxCorruptError()
    result[key] = value
  }
  return result
}
