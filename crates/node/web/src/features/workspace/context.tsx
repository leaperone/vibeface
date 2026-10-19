"use client"

import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import type { ReadyConnection } from "@/lib/node"
import { messageOf } from "@/lib/node"
import { mergeProgress, type LiveProgress } from "@/lib/progress"
import { isQueuedReceipt, prepareSend } from "../chat/protocol"
import { conversationHref } from "./entry-url"
import { isIncomplete, mergeBasis } from "./labels"
import {
  frontierMap, inputText, parseConversation, parseNodes, parseNotifications,
  parsePendingMessages, parseBot, parseBots, parseCreateBot, parsePendingBot, record,
  parseArtifactPage, hasQueuedMessage,
} from "./model"
import { networkKey, newId, readStorage, writeStorage } from "./storage"
import type {
  AutonomyMode, ConversationPayload, NodeInfo, NotificationRecord,
  PendingMessage, WorkspaceView, BotRecord, CreateBotRequest, PendingBot, BotProject, PendingCheck,
} from "./types"

function useWorkspaceState(connection: ReadyConnection) {
  const networkId = connection.principal.network_id
  const clientId = connection.principal.subject_id
  const [booted, setBooted] = useState(false)
  const [error, setError] = useState("")
  const [noticeText, setNoticeText] = useState("")
  const [view, setView] = useState<WorkspaceView>("chat")
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [conversationId, setConversationId] = useState("")
  const [data, setData] = useState<ConversationPayload | null>(null)
  const [notifications, setNotifications] = useState<NotificationRecord[]>([])
  const [pendingMessages, setPendingMessages] = useState<PendingMessage[]>([])
  const [bots, setBots] = useState<BotRecord[]>([])
  const [pendingBot, setPendingBot] = useState<PendingBot | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const [messageBusy, setMessageBusy] = useState(false)
  const [liveProgress, setLiveProgress] = useState<LiveProgress[]>([])
  const [notificationButton, setNotificationButton] = useState({ label: "开启浏览器提醒", disabled: false })
  const alive = useRef(true)
  const activeId = useRef("")
  const dataRef = useRef<ConversationPayload | null>(null)
  const readConnection = useRef<ReadyConnection | null>(null)
  const readSequence = useRef(0)
  const messagesRef = useRef<PendingMessage[]>([])
  const botCreationRef = useRef<PendingBot | null>(null)
  const notificationsRef = useRef<NotificationRecord[]>([])
  const inFlight = useRef(new Set<string>())
  const refreshingRef = useRef(false)
  const refreshingConnection = useRef<ReadyConnection | null>(null)
  const refreshAgain = useRef(false)
  const refreshTimer = useRef<number | null>(null)
  const refreshRef = useRef<() => Promise<void>>(async () => {})
  const current = () => alive.current && connection.isCurrent()
  const requireCurrent = () => { if (!current()) throw new Error("连接已切换，请在当前入口重新核对。") }
  const requireContext = (id = activeId.current) => {
    requireCurrent()
    const context = dataRef.current
    if (readConnection.current !== connection || !id || id !== activeId.current || context?.conversation?.conversation_id !== id || isIncomplete(context.missing)) throw new Error("先选择并读取上下文完整的对话。")
    return context
  }
  const requireBotContext = (id = activeId.current) => {
    if (!bots.some((bot) => bot.conversation_id === id)) throw new Error("请先选择机器人后再操作。")
    return requireContext(id)
  }
  const showError = (caught: unknown) => { if (current()) setError(messageOf(caught)) }
  const requireWritable = () => {
    requireCurrent()
    if (!booted) throw new Error("本地待核对记录尚未读取，已暂停写入。")
    if (readConnection.current !== connection) throw new Error("正在补读当前设备的记录，已暂停写入。")
  }
  const scoped = (key: string) => networkKey(`${key}.${clientId}`, networkId)
  const readFrontiers = () => frontierMap(readStorage<unknown>(networkKey("frontiers", networkId), {}, true))
  const replaceMessages = (next: PendingMessage[]) => {
    requireWritable()
    writeStorage(scoped("pending-messages"), next, true)
    messagesRef.current = next
    if (current()) setPendingMessages(next)
  }
  const selectConversation = (id: string) => {
    requireCurrent()
    activeId.current = id
    readSequence.current += 1
    dataRef.current = null
    setConversationId(id)
    setData(null)
    setLiveProgress([])
    window.history.replaceState(null, "", conversationHref(window.location.href, id))
  }
  const readConversation = async (id: string) => {
    const sequence = ++readSequence.current
    const basis = readFrontiers()[id] ?? []
    const next = parseConversation(await connection.rpc("conversation", { conversation_id: id, basis_heads: basis }), id)
    requireCurrent()
    if (activeId.current !== id || sequence !== readSequence.current) throw new Error("对话读取已更新，请重新核对。")
    if (!isIncomplete(next.missing)) {
      const latest = readFrontiers()
      const concurrent = (latest[id] ?? []).filter((head) => !basis.includes(head))
      writeStorage(networkKey("frontiers", networkId), { ...latest, [id]: [...new Set([...next.heads, ...concurrent])] }, true)
    }
    dataRef.current = next
    setData(next)
    if (current() && readConnection.current === connection && messagesRef.current.some((item) => item.conversation_id === id && hasQueuedMessage(next.scheduler, item))) {
      replaceMessages(messagesRef.current.filter((item) => item.conversation_id !== id || !hasQueuedMessage(next.scheduler, item)))
    }
    setLiveProgress((previous) => previous.filter((progress) => {
      if (progress.kind === "native") return next.native_sessions.some((item) => item.target_node === progress.target_node && item.work_id === progress.work_id && item.thread_id === progress.thread_id && item.turn_id === (progress.turn_id ?? null))
      const job = next.scheduler?.jobs.find((item) => item.trigger === progress.trigger)
      return !job || (!job.reply_message_id || !next.messages.some((message) => message.message_id === job.reply_message_id)) && !["completed", "failed", "cancelled", "interrupted", "rejected"].includes(job.state)
    }))
    if (current() && readConnection.current === connection && view === "chat" && !document.hidden && !isIncomplete(next.missing)) {
      const seen = notificationsRef.current.filter((item) => item.conversation_id === id)
      for (const notification of seen) {
        try {
          await connection.rpc("ack", { notification_id: notification.notification_id })
          requireCurrent()
          notificationsRef.current = notificationsRef.current.filter((item) => item.notification_id !== notification.notification_id)
          setNotifications(notificationsRef.current)
        } catch (caught) { showError(caught); break }
      }
    }
    return next
  }
  const refreshNotificationButton = () => {
    if (!("Notification" in window)) { setNotificationButton({ label: "浏览器不支持提醒", disabled: true }); return }
    const enabled = readStorage(networkKey("notify", networkId), false)
    setNotificationButton(Notification.permission === "granted"
      ? { label: enabled ? "浏览器提醒已开启" : "启用此网络提醒", disabled: enabled }
      : Notification.permission === "denied" ? { label: "请在浏览器设置允许提醒", disabled: true }
      : { label: "开启浏览器提醒", disabled: false })
  }
  const notifyBrowser = (items: NotificationRecord[]) => {
    if (!("Notification" in window) || Notification.permission !== "granted" || !readStorage(networkKey("notify", networkId), false)) return
    const seen = new Set(readStorage<string[]>(networkKey("seen", networkId), []))
    for (const item of items) {
      if (seen.has(item.notification_id)) continue
      seen.add(item.notification_id)
      try {
        const popup = new Notification("Vibeface · 进展", { body: item.text, tag: item.notification_id })
        popup.onclick = () => {
          if (!current()) { popup.close(); return }
          window.focus(); selectConversation(item.conversation_id); setView("chat")
          void refreshRef.current().catch(showError)
          popup.close()
        }
      } catch { /* The persisted inbox remains available when browser notifications fail. */ }
    }
    writeStorage(networkKey("seen", networkId), [...seen].slice(-1000))
  }
  const scheduleRefresh = () => {
    if (!current() || refreshTimer.current !== null) return
    refreshTimer.current = window.setTimeout(() => {
      refreshTimer.current = null
      void refreshRef.current().catch(showError)
    }, 350)
  }
  const refresh = async () => {
    requireCurrent()
    if (refreshingRef.current && refreshingConnection.current === connection) { refreshAgain.current = true; return }
    refreshingRef.current = true
    refreshingConnection.current = connection
    setRefreshing(true)
    try {
      const [rawNodes, rawNotifications, rawBots] = await Promise.all([
        connection.rpc("nodes"), connection.rpc("notifications"), connection.rpc("bots"),
      ])
      requireCurrent()
      const nextNodes = parseNodes(rawNodes), nextNotifications = parseNotifications(rawNotifications)
      const nextBots = parseBots(rawBots)
      setNodes(nextNodes); setNotifications(nextNotifications); setBots(nextBots)
      notificationsRef.current = nextNotifications
      const selected = readStorage<string | null>(scoped("selected-bot"), null)
      const bot = nextBots.find((item) => item.conversation_id === activeId.current) ?? nextBots.find((item) => item.bot_id === selected) ?? nextBots[0]
      if (activeId.current !== (bot?.conversation_id ?? "")) selectConversation(bot?.conversation_id ?? "")
      if (activeId.current) await readConversation(activeId.current)
      requireCurrent()
      readConnection.current = connection
      notifyBrowser(nextNotifications)
      refreshNotificationButton()
    } catch (caught) {
      if (current()) { readConnection.current = null; dataRef.current = null; setData(null) }
      throw caught
    } finally {
      if (refreshingConnection.current === connection) refreshingRef.current = false
      if (current()) {
        setRefreshing(false)
        if (refreshAgain.current) { refreshAgain.current = false; scheduleRefresh() }
      }
    }
  }
  refreshRef.current = refresh

  const loadPending = () => {
    const storedMessages = parsePendingMessages(readStorage<unknown>(scoped("pending-messages"), [], true))
    const rawBot = readStorage<unknown>(scoped("pending-bot"), null, true)
    const storedBot = rawBot == null ? null : parsePendingBot(rawBot)
    messagesRef.current = storedMessages; setPendingMessages(storedMessages)
    botCreationRef.current = storedBot; setPendingBot(storedBot)
  }

  useEffect(() => {
    alive.current = true
    inFlight.current = new Set()
    setMessageBusy(false); setLiveProgress([]); setError(""); setNoticeText("")
    try {
      const params = new URL(window.location.href).searchParams
      activeId.current = params.get("conversation") ?? ""
      loadPending()
      setConversationId(activeId.current)
      const frontiers = readFrontiers(), heads = frontiers[activeId.current] ?? []
      const basis = mergeBasis(heads, params.get("basis"))
      if (activeId.current && basis !== heads) writeStorage(networkKey("frontiers", networkId), { ...frontiers, [activeId.current]: basis }, true)
      setBooted(true)
    } catch (caught) { showError(caught) }
    return () => { alive.current = false }
  }, [connection])

  useEffect(() => {
    if (!booted) return
    void refreshRef.current().catch(showError)
    const unsubscribe = connection.subscribe((event) => {
      if (!current()) return
      if (event.type === "invalidate") { scheduleRefresh(); return }
      if (event.conversation_id !== activeId.current) return
      setLiveProgress((previous) => {
        const next = mergeProgress(previous, event)
        if (next.some((item) => item.needsRefresh && !previous.find((prior) => prior.key === item.key)?.needsRefresh)) scheduleRefresh()
        return next
      })
    })
    const timer = window.setInterval(scheduleRefresh, 15000)
    const visible = () => { if (!document.hidden) scheduleRefresh() }
    document.addEventListener("visibilitychange", visible)
    return () => {
      unsubscribe(); window.clearInterval(timer)
      if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current)
      refreshTimer.current = null
      document.removeEventListener("visibilitychange", visible)
    }
  }, [booted, connection])

  const withMutation = async <T,>(key: string, work: () => Promise<T>) => {
    requireWritable()
    const mutations = inFlight.current
    if (mutations.has(key)) throw new Error("原请求尚未结束，请等待并核对结果。")
    mutations.add(key)
    try { return await work() } finally { mutations.delete(key) }
  }
  const run = async (action: () => Promise<void>) => {
    if (current()) setError("")
    try { await action() } catch (caught) { showError(caught) }
  }
  const requireControlRefresh = () => {
    if (!current()) return
    readConnection.current = null
    readSequence.current += 1
    setNoticeText("控制结果尚未确认，正在核对原请求。")
    scheduleRefresh()
  }
  const submitMessage = async (text: string) => {
    const context = requireContext(), id = activeId.current
    const prior = messagesRef.current.find((item) => item.conversation_id === id) ?? null
    const prepared = prepareSend({ conversationId: id, text, schedulerPeerId: null, entryNode: connection.entry.peer_id, pending: prior, heads: context.heads, reportResults: true })
    if (!prepared.ok) throw new Error(prepared.error)
    replaceMessages([...messagesRef.current.filter((item) => item.conversation_id !== id), prepared.pending])
    try {
      const receipt = await connection.rpc("message", prepared.body)
      requireCurrent()
      const response = record(receipt)
      if (response.accepted === true && response.message_id === prepared.pending.message_id && typeof response.scheduler_node === "string" && response.scheduler_node && (prepared.pending.scheduler_node === null || prepared.pending.scheduler_node === response.scheduler_node)) {
        const schedulerNode = response.scheduler_node
        replaceMessages(messagesRef.current.map((item) => item.message_id === prepared.pending.message_id ? { ...item, scheduler_node: schedulerNode } : item))
      }
      if (!isQueuedReceipt(receipt, prepared.pending)) {
        if (activeId.current === id) await refreshRef.current().catch(showError)
        return false
      }
      replaceMessages(messagesRef.current.filter((item) => item.message_id !== prepared.pending.message_id))
      setNoticeText("")
      if (activeId.current === id) await refreshRef.current().catch(showError)
      return true
    } catch (caught) {
      if (current() && caught instanceof Error && "responded" in caught && caught.responded === true) {
        const original = messagesRef.current.find((item) => item.message_id === prepared.pending.message_id)
        if (original) replaceMessages(messagesRef.current.map((item) => item === original ? { ...item, rejected: { entry_node: connection.entry.peer_id } } : item))
      }
      throw caught
    }
  }
  const sameProject = (left?: BotProject, right?: BotProject) => left?.target_node === right?.target_node && left?.cwd === right?.cwd
  const clearPendingBot = () => {
    writeStorage(scoped("pending-bot"), null, true)
    botCreationRef.current = null; setPendingBot(null)
  }
  const checkRejectedBot = async (pending: PendingBot): Promise<BotRecord | null> => {
    if (pending.submission?.state !== "rejected" || pending.submission.entry_node !== connection.entry.peer_id) throw new Error("请回原入口核对机器人创建结果，原配置仍已保留。")
    const known = parseBots(await connection.rpc("bots"))
    requireCurrent()
    const match = known.find((bot) => bot.bot_id === pending.bot_id || bot.conversation_id === pending.conversation_id)
    if (match && (match.bot_id !== pending.bot_id || match.conversation_id !== pending.conversation_id || match.name !== pending.name || match.instructions !== pending.instructions || !sameProject(match.project, pending.project))) throw new Error("已找到相关机器人定义，请保留原请求并核对配置。")
    clearPendingBot()
    if (!match) setNoticeText("已确认机器人没有创建，可以修改配置后重新提交。")
    return match ?? null
  }
  const selectedBot = bots.find((bot) => bot.conversation_id === conversationId) ?? null
  const selectBot = async (bot: BotRecord) => {
    requireCurrent()
    writeStorage(scoped("selected-bot"), bot.bot_id)
    selectConversation(bot.conversation_id)
    setView("chat")
    await refreshRef.current()
  }
  const sendMessage = (input: string) => withMutation(`message:${activeId.current}`, async () => {
    if (!bots.some((bot) => bot.conversation_id === activeId.current)) throw new Error("请先选择机器人。")
    const text = inputText(input, "消息内容", 65536)
    setMessageBusy(true)
    try { return await submitMessage(text) } finally { if (current()) setMessageBusy(false) }
  })
  const incomplete = !data || isIncomplete(data.missing)
  const messagePending = pendingMessages.find((item) => item.conversation_id === conversationId) ?? null
  const chatSlot = { conversationId: conversationId || null, missing: incomplete }

  return {
    info: connection.info, generation: connection.generation, isCurrent: current, connectionOnline: current(), connectionText: current() ? "已建立端到端加密连接" : "连接已断开，正在恢复只读订阅；未确认操作不会自动重发。",
    error, noticeText, view, setView, nodes, conversationId, data, notifications, bots, selectedBot, pendingBot,
    refreshing, incomplete, chatSlot, messagePending, messageBusy, liveProgress,
    writable: booted && current() && readConnection.current === connection,
    writeNotice: readConnection.current !== connection ? "正在补读当前设备的记录，已暂停写入。" : "",
    syncText: !data ? "发一条消息开始对话。" : incomplete
      ? "对话还在同步。"
      : `此入口已取得已知上下文，共 ${data.heads.length} 个历史位置。其他离线设备可能仍有未同步记录。`,
    refresh, run, announce: (notice: string) => { if (current()) setNoticeText(notice) }, notificationButton,
    selectBot,
    createBot: (input: { name: string; instructions: string; project?: BotProject }) => withMutation("create-bot", async () => {
      const name = inputText(input.name, "机器人名称", 512), instructions = input.instructions.trim()
      if (new TextEncoder().encode(instructions).length > 8192) throw new Error("职责说明不能超过 8192 字节。")
      if (new TextEncoder().encode(JSON.stringify([name, instructions, input.project ?? null])).length > 12000) throw new Error("机器人配置不能超过 12000 字节，请缩短名称、职责或项目路径。")
      const prior = botCreationRef.current
      if (prior && (prior.name !== name || prior.instructions !== instructions || !sameProject(prior.project, input.project))) throw new Error("机器人创建结果尚未确认，请保留原配置重试。")
      if (prior?.submission?.state === "rejected") {
        const known = await checkRejectedBot(prior)
        if (known) { await selectBot(known); return }
        throw new Error("已确认机器人没有创建，可以修改配置后重新提交。")
      }
      const pending: PendingBot = prior ?? { bot_id: newId(), conversation_id: newId(), name, instructions, project: input.project, submission: { state: "unconfirmed", entry_node: connection.entry.peer_id } }
      const request: CreateBotRequest = parseCreateBot(pending)
      writeStorage(scoped("pending-bot"), pending, true)
      botCreationRef.current = pending; setPendingBot(pending)
      let response: unknown
      try { response = await connection.rpc("create_bot", request) }
      catch (caught) {
        if (!prior && current() && caught instanceof Error && "responded" in caught && caught.responded === true) {
          const rejected: PendingBot = { ...pending, submission: { state: "rejected", entry_node: connection.entry.peer_id } }
          writeStorage(scoped("pending-bot"), rejected, true)
          botCreationRef.current = rejected; setPendingBot(rejected)
          const known = await checkRejectedBot(rejected)
          if (known) { await selectBot(known); return }
          throw new Error(`${messageOf(caught)} 已确认没有创建，可以修改配置后重新提交。`)
        }
        throw caught
      }
      const bot = parseBot(record(response).bot)
      requireCurrent()
      if (bot.bot_id !== request.bot_id || bot.conversation_id !== request.conversation_id || bot.name !== request.name || bot.instructions !== request.instructions || !sameProject(bot.project, request.project)) throw new Error("机器人创建回执与原配置不符，请核对原请求。")
      clearPendingBot()
      await selectBot(bot)
    }),
    sendMessage,
    cancelManager: (trigger: string) => withMutation(`cancel-manager:${trigger}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      const receipt = record(await connection.rpc("cancel_manager", { conversation_id: id, trigger }))
      requireCurrent()
      const job = record(receipt.job)
      if (receipt.publication_pending === true) setNoticeText("本轮已有结果，正在补齐发布；未提交停止请求。")
      else if (receipt.cancel_requested === true && job.state === "cancelled") setNoticeText("本轮回复已停止。已派出的任务和原生工作会继续。")
      else if (receipt.cancel_requested === true) setNoticeText("已提交停止本轮回复。已派出的任务和原生工作会继续，最终状态以核对结果为准。")
      else setNoticeText("本轮当前状态未接受停止请求，请核对最新状态。")
      if (activeId.current === id) await refreshRef.current()
    }),
    setAutonomy: (mode: AutonomyMode) => withMutation(`autonomy:${activeId.current}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      const version = context.autonomy.version
      if (version === null) throw new Error("后续授权记录无法读取，请先核对对话。")
      try { await connection.rpc("set_autonomy", { conversation_id: id, mode, expected_version: version }) }
      catch (caught) {
        if (messageOf(caught).includes("refresh")) {
          if (activeId.current === id) await refreshRef.current()
          throw new Error("后续授权已在别处更改，已刷新。请确认当前设置后再改。")
        }
        requireControlRefresh()
        throw caught
      }
      requireCurrent()
      setNoticeText(mode === "off" ? "已停止这个对话的后续。" : mode === "report" ? "后续只汇报结果，不再继续动手。" : "已允许 Agent 收到结果后继续工作。")
      if (activeId.current === id) await refreshRef.current()
    }),
    cancelAgentTask: (taskRef: string) => withMutation(`cancel-task:${taskRef}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      if (!context.agent_tasks.some((task) => task.task_ref === taskRef)) throw new Error("未找到当前任务，请先核对进展。")
      try { await connection.rpc("cancel_agent_task", { conversation_id: id, task_ref: taskRef }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      setNoticeText("已提交取消任务请求，最终结果以目标设备回报为准。")
      if (activeId.current === id) await refreshRef.current()
    }),
    interruptNative: (workId: string, targetNode: string) => withMutation(`native-control:${targetNode}:${workId}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      const session = context.native_sessions.find((item) => item.work_id === workId && item.target_node === targetNode)
      if (!session?.turn_id || !["running", "waiting"].includes(session.state)) throw new Error("此工作没有可中断的当前轮次，请先核对进展。")
      try { await connection.rpc("native_interrupt", { conversation_id: id, work_id: workId, target_node: targetNode }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      setNoticeText("已提交原生停止请求，是否结束以原生工作最终状态为准。")
      if (activeId.current === id) await refreshRef.current()
    }),
    respondNative: (workId: string, targetNode: string, requestId: string | number, response: unknown) => withMutation(`native-control:${targetNode}:${workId}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      const session = context.native_sessions.find((item) => item.work_id === workId && item.target_node === targetNode)
      const request = session?.pending_requests.find((item) => item.request_id === requestId)
      if (!session || !["running", "waiting"].includes(session.state) || !request) throw new Error("未找到当前待回复请求，请先核对进展。")
      if (request.questions?.some((question) => question.is_secret)) throw new Error("敏感输入请在原生工作设备填写。")
      try { await connection.rpc("native_respond", { conversation_id: id, work_id: workId, target_node: targetNode, request_id: requestId, response }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      setNoticeText("本人决定已提交，请查看原生工作的最新状态。")
      if (activeId.current === id) await refreshRef.current()
    }),
    reconcileCheck: (check: PendingCheck) => withMutation(`pending-check:${check.target_node}:${check.check_id}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      const result = record(await connection.rpc("reconcile", { conversation_id: id, target_node: check.target_node, check_id: check.check_id }))
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
      return result
    }),
    resolveCheck: (check: PendingCheck, evidence: string, outcome?: string) => withMutation(`pending-check:${check.target_node}:${check.check_id}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      if (new TextEncoder().encode(evidence.trim()).length < 12) throw new Error("请填写至少 12 字节的本人核查证据。")
      if (check.kind === "native_turn" && !outcome) throw new Error("请选择原生工作的实际结果。")
      await connection.rpc("resolve", { conversation_id: id, target_node: check.target_node, check_id: check.check_id, evidence: evidence.trim(), ...(outcome ? { outcome } : {}) })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    readArtifact: async (hash: string, offset = 0) => {
      const id = activeId.current
      requireCurrent()
      const result = parseArtifactPage(await connection.rpc("read_artifact", { conversation_id: id, blob_hash: hash, offset, limit: 65536 }), hash, offset)
      requireCurrent()
      if (activeId.current !== id) throw new Error("对话已切换，请在原对话中重新打开产物。")
      return result
    },
    releaseMessagePending: () => withMutation(`message:${activeId.current}`, async () => {
      requireCurrent()
      const id = activeId.current, original = messagesRef.current.find((item) => item.conversation_id === id)
      if (!original) throw new Error("原消息请求尚未结束，请先核对。")
      if (original.entry_node !== connection.entry.peer_id || original.rejected?.entry_node !== connection.entry.peer_id) throw new Error("需要原入口明确返回错误，再在同一连接核对完整对话。超时或断线后不能据此解除，请原样重试并核对。")
      const complete = await readConversation(id)
      requireContext(id)
      if (messagesRef.current.find((item) => item.conversation_id === id) !== original || complete.messages.some((item) => item.message_id === original.message_id)) throw new Error("已找到原消息或待核对记录变化，请保留原记录并核对处理结果。")
      replaceMessages(messagesRef.current.filter((item) => item !== original))
      setNoticeText("已解除本地待核对记录，未重发消息。")
    }),
    enableBrowserNotifications: async () => {
      requireCurrent()
      if (!("Notification" in window)) return
      const permission = Notification.permission === "granted" ? "granted" : await Notification.requestPermission()
      requireCurrent()
      writeStorage(networkKey("notify", networkId), permission === "granted")
      if (permission === "granted") { writeStorage(networkKey("seen", networkId), notificationsRef.current.map((item) => item.notification_id)); setNoticeText("浏览器提醒已开启。已有未读提醒保留在对话列表中。") }
      refreshNotificationButton()
    },
  }
}

const WorkspaceContext = createContext<ReturnType<typeof useWorkspaceState> | null>(null)
export function useWorkspace() {
  const value = useContext(WorkspaceContext)
  if (!value) throw new Error("工作空间屏幕需要放在 WorkspaceProvider 中。")
  return value
}
export function WorkspaceProvider({ connection, children }: { connection: ReadyConnection; children: ReactNode }) {
  const value = useWorkspaceState(connection)
  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>
}
