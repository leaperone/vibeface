"use client"

import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import type { ReadyConnection } from "@/lib/node"
import { mergeProgress, type LiveProgress } from "@/lib/progress"
import { isQueuedReceipt, prepareSend } from "../chat/protocol"
import { conversationHref } from "./entry-url"
import { isIncomplete, mergeBasis, missingCount, stateNames } from "./labels"
import {
  frontierMap, inputText, parseConversation, parseConversations, parseNodes, parseNotifications, parseOperation,
  parsePending, parsePendingMessages, parsePendingConversation, parsePendingTasks, parseSession, record,
  parseArtifactPage, hasQueuedMessage, canSelectScheduler,
} from "./model"
import { errorText, parseReconcile, reconcileDecision, type ReconcileRecord } from "./reconcile"
import { networkKey, newId, readStorage, writeStorage } from "./storage"
import type {
  ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, OperateAction,
  OperationRequest, PendingOperation, PendingMessage, PendingConversation, PendingTask, SessionRecord, WorkspaceView, FollowupMode, TaskRecord, TaskControlRequest,
} from "./types"

function useWorkspaceState(connection: ReadyConnection) {
  const networkId = connection.principal.network_id
  const clientId = connection.principal.subject_id
  const canAutoFollowup = connection.info.roles.includes("gateway")
  const [booted, setBooted] = useState(false)
  const [writeState, setWriteState] = useState<"checking" | "writable" | "readonly" | "unsupported">("checking")
  const writer = useRef(false)
  const [error, setError] = useState("")
  const [noticeText, setNoticeText] = useState("")
  const [view, setView] = useState<WorkspaceView>("chat")
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [conversations, setConversations] = useState<ConversationSummary[]>([])
  const [conversationId, setConversationId] = useState("")
  const [data, setData] = useState<ConversationPayload | null>(null)
  const [notifications, setNotifications] = useState<NotificationRecord[]>([])
  const [pending, setPending] = useState<PendingOperation[]>([])
  const [pendingMessages, setPendingMessages] = useState<PendingMessage[]>([])
  const [pendingConversation, setPendingConversation] = useState<PendingConversation | null>(null)
  const [pendingTasks, setPendingTasks] = useState<PendingTask[]>([])
  const [actual, setActual] = useState<Record<string, unknown>>({})
  const [refreshing, setRefreshing] = useState(false)
  const [messageBusy, setMessageBusy] = useState(false)
  const [liveProgress, setLiveProgress] = useState<LiveProgress[]>([])
  const [schedulerNodeId, setSchedulerNodeId] = useState("")
  const [notificationButton, setNotificationButton] = useState({ label: "开启浏览器提醒", disabled: false })
  const alive = useRef(true)
  const activeId = useRef("")
  const dataRef = useRef<ConversationPayload | null>(null)
  const readSequence = useRef(0)
  const pendingRef = useRef<PendingOperation[]>([])
  const messagesRef = useRef<PendingMessage[]>([])
  const creationsRef = useRef<PendingConversation | null>(null)
  const tasksRef = useRef<PendingTask[]>([])
  const notificationsRef = useRef<NotificationRecord[]>([])
  const inFlight = useRef(new Set<string>())
  const refreshingRef = useRef(false)
  const refreshAgain = useRef(false)
  const refreshTimer = useRef<number | null>(null)
  const refreshRef = useRef<() => Promise<void>>(async () => {})
  const current = () => alive.current && connection.isCurrent()
  const requireCurrent = () => { if (!current()) throw new Error("连接已切换，请在当前入口重新核对。") }
  const requireContext = (id = activeId.current) => {
    requireCurrent()
    const context = dataRef.current
    if (!id || id !== activeId.current || context?.conversation?.conversation_id !== id || isIncomplete(context.missing)) throw new Error("先选择并读取上下文完整的会话。")
    return context
  }
  const showError = (caught: unknown) => { if (current()) setError(errorText(caught)) }
  const requireWriter = () => {
    requireCurrent()
    if (!writer.current) throw new Error("此页面只读。关闭正在操作此设备的其他页面，再重新连接。浏览器需支持 Web Locks。")
  }
  const scoped = (key: string) => networkKey(`${key}.${clientId}`, networkId)
  const readFrontiers = () => frontierMap(readStorage<unknown>(networkKey("frontiers", networkId), {}, true))
  const replacePending = (next: PendingOperation[]) => {
    requireWriter()
    writeStorage(scoped("pending"), next, true)
    pendingRef.current = next
    if (current()) setPending(next)
  }
  const replaceMessages = (next: PendingMessage[]) => {
    requireWriter()
    writeStorage(scoped("pending-messages"), next, true)
    messagesRef.current = next
    if (current()) setPendingMessages(next)
  }
  const removePending = (id: string) => replacePending(pendingRef.current.filter((item) => item.operation_id !== id))
  const selectConversation = (id: string) => {
    requireCurrent()
    activeId.current = id
    readSequence.current += 1
    dataRef.current = null
    setConversationId(id)
    setData(null)
    setActual({})
    setLiveProgress([])
    window.history.replaceState(null, "", conversationHref(window.location.href, id))
  }
  const readConversation = async (id: string) => {
    const sequence = ++readSequence.current
    const basis = readFrontiers()[id] ?? []
    const next = parseConversation(await connection.rpc("conversation", { conversation_id: id, basis_heads: basis }), id)
    requireCurrent()
    if (activeId.current !== id || sequence !== readSequence.current) throw new Error("会话读取已更新，请重新核对。")
    if (!isIncomplete(next.missing)) {
      const latest = readFrontiers()
      const concurrent = (latest[id] ?? []).filter((head) => !basis.includes(head))
      writeStorage(networkKey("frontiers", networkId), { ...latest, [id]: [...new Set([...next.heads, ...concurrent])] }, true)
    }
    dataRef.current = next
    setData(next)
    if (writer.current && messagesRef.current.some((item) => item.conversation_id === id && hasQueuedMessage(next.scheduler, item))) {
      replaceMessages(messagesRef.current.filter((item) => item.conversation_id !== id || !hasQueuedMessage(next.scheduler, item)))
    }
    setLiveProgress((previous) => previous.filter((progress) => {
      if (progress.kind === "execution") return next.tasks.some((task) => task.binding && task.binding.target_node === progress.target_node && task.binding.session_id === progress.session_id && (!task.binding.operation_id || task.binding.operation_id === progress.operation_id))
      const job = next.scheduler?.jobs.find((item) => item.trigger === progress.trigger)
      return !job || (!job.reply_message_id || !next.messages.some((message) => message.message_id === job.reply_message_id)) && !["completed", "failed", "cancelled", "unknown", "rejected"].includes(job.state)
    }))
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
        const popup = new Notification("Vibeface · 任务进展", { body: item.text, tag: item.notification_id })
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
    if (refreshingRef.current) { refreshAgain.current = true; return }
    refreshingRef.current = true
    setRefreshing(true)
    try {
      const [rawNodes, rawConversations, rawNotifications] = await Promise.all([
        connection.rpc("nodes"), connection.rpc("conversations"), connection.rpc("notifications"),
      ])
      requireCurrent()
      const nextNodes = parseNodes(rawNodes), nextConversations = parseConversations(rawConversations), nextNotifications = parseNotifications(rawNotifications)
      setNodes(nextNodes); setConversations(nextConversations); setNotifications(nextNotifications)
      notificationsRef.current = nextNotifications
      if (!activeId.current && nextConversations[0]) selectConversation(nextConversations[0].conversation_id)
      if (activeId.current) await readConversation(activeId.current)
      requireCurrent()
      setSchedulerNodeId((previous) => {
        const choices = nextNodes.filter(canSelectScheduler)
        return choices.some((node) => node.node_id === previous) ? previous : choices[0]?.node_id ?? ""
      })
      notifyBrowser(nextNotifications)
      refreshNotificationButton()
    } catch (caught) {
      if (current()) { dataRef.current = null; setData(null) }
      throw caught
    } finally {
      refreshingRef.current = false
      if (current()) {
        setRefreshing(false)
        if (refreshAgain.current) { refreshAgain.current = false; scheduleRefresh() }
      }
    }
  }
  refreshRef.current = refresh

  const loadPending = () => {
    const storedPending = parsePending(readStorage<unknown>(scoped("pending"), [], true), networkId)
    const storedMessages = parsePendingMessages(readStorage<unknown>(scoped("pending-messages"), [], true))
    const storedConversation = parsePendingConversation(readStorage<unknown>(scoped("pending-conversation"), null, true))
    const storedTasks = parsePendingTasks(readStorage<unknown>(scoped("pending-tasks"), [], true))
    pendingRef.current = storedPending; setPending(storedPending)
    messagesRef.current = storedMessages; setPendingMessages(storedMessages)
    creationsRef.current = storedConversation; setPendingConversation(storedConversation)
    tasksRef.current = storedTasks; setPendingTasks(storedTasks)
  }

  useEffect(() => {
    alive.current = true
    try {
      const params = new URL(window.location.href).searchParams
      activeId.current = params.get("conversation") ?? ""
      setConversationId(activeId.current)
      loadPending()
      const frontiers = readFrontiers(), heads = frontiers[activeId.current] ?? []
      const basis = mergeBasis(heads, params.get("basis"))
      if (activeId.current && basis !== heads) writeStorage(networkKey("frontiers", networkId), { ...frontiers, [activeId.current]: basis }, true)
      setBooted(true)
    } catch (caught) { showError(caught) }
    return () => { alive.current = false }
  }, [connection])

  useEffect(() => {
    let disposed = false
    let release = () => {}
    writer.current = false
    if (!("locks" in navigator)) { setWriteState("unsupported"); return }
    void navigator.locks.request(scoped("workspace-writer"), { mode: "exclusive", ifAvailable: true }, async (lock) => {
      if (disposed) return
      if (!lock) { setWriteState("readonly"); return }
      loadPending()
      writer.current = true
      setWriteState("writable")
      await new Promise<void>((resolve) => { release = resolve })
      writer.current = false
    }).catch((caught: unknown) => { if (!disposed) { setWriteState("unsupported"); showError(caught) } })
    return () => { disposed = true; writer.current = false; release() }
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
    requireWriter()
    if (!booted) throw new Error("本地待核对记录尚未读取，已暂停写入。")
    if (inFlight.current.has(key)) throw new Error("原请求尚未结束，请等待并核对结果。")
    inFlight.current.add(key)
    try { return await work() } finally { inFlight.current.delete(key) }
  }
  const run = async (action: () => Promise<void>) => {
    if (current()) setError("")
    try { await action() } catch (caught) { showError(caught) }
  }
  const submitTaskControl = async (request: TaskControlRequest) => {
    const result = parseOperation(record(await connection.rpc("task_control", request)).operation)
    requireCurrent()
    if (result.request.operation_id !== request.operation_id || result.request.target_node !== request.target_node || result.request.conversation_id !== request.conversation_id || result.request.session_id !== request.session_id) throw new Error("控制回执与原请求不符，已保留原编号待核对。")
    removePending(request.operation_id)
    setNoticeText(request.action === "interrupt" ? "停止请求已入账，当前执行是否结束以执行端终态为准。" : "本人操作已入账，请查看任务状态。")
    if (activeId.current === request.conversation_id) await refreshRef.current()
  }
  const incomplete = !data || isIncomplete(data.missing)
  const messagePending = pendingMessages.find((item) => item.conversation_id === conversationId) ?? null
  const selectedScheduler = nodes.find((node) => node.node_id === (messagePending?.scheduler_node ?? schedulerNodeId))
  const chatSlot = {
    conversationId: conversationId || null, schedulerNodeId: selectedScheduler?.node_id ?? null,
    missing: incomplete, schedulerOnline: Boolean(selectedScheduler?.online && !selectedScheduler.revoked), schedulerAllowed: canSelectScheduler(selectedScheduler),
  }

  return {
    info: connection.info, connectionOnline: current(), connectionText: current() ? "已建立端到端加密连接" : "连接已断开，正在恢复只读订阅；未确认操作不会自动重发。",
    error, noticeText, view, setView, nodes, conversations, conversationId, data, notifications, pending, actual,
    canAutoFollowup, pendingConversation, pendingTask: pendingTasks.find((item) => item.conversation_id === conversationId) ?? null,
    refreshing, schedulerNodeId, setSchedulerNodeId, incomplete, chatSlot, messagePending, messageBusy, liveProgress,
    writable: writeState === "writable" && current(),
    writeNotice: writeState === "readonly" ? "另一个页面正在操作此浏览器设备。此页只读，关闭其他页面后重新连接。"
      : writeState === "unsupported" ? "浏览器未能提供跨页面写锁，此页只读。请使用支持 Web Locks 的安全浏览器入口。"
      : writeState === "checking" ? "正在核对浏览器设备的写入权限。" : "",
    syncText: !data ? "选择或创建会话后，可发消息和派发任务。" : incomplete
      ? `上下文尚未完整，已暂停派发。缺少事件 ${missingCount(data.missing.events)}，附件 ${missingCount(data.missing.blobs)}，冲突 ${missingCount(data.missing.conflicts)}。`
      : `此入口已取得已知上下文，共 ${data.heads.length} 个历史位置。其他离线设备可能仍有未同步记录。`,
    selectConversation, refresh, run, announce: setNoticeText, notificationButton,
    createConversation: (proposedId: string, inputTitle: string) => withMutation("create-conversation", async () => {
      const title = inputText(inputTitle, "会话名称", 512)
      const prior = creationsRef.current
      if (prior && prior.title !== title) throw new Error(`还有待确认的会话“${prior.title}”，请保留原名称重试。`)
      const request = prior ?? { conversation_id: proposedId, title }
      writeStorage(scoped("pending-conversation"), request, true)
      creationsRef.current = request; setPendingConversation(request)
      await connection.rpc("create_conversation", request)
      requireCurrent()
      writeStorage(scoped("pending-conversation"), null, true)
      creationsRef.current = null; setPendingConversation(null)
      selectConversation(request.conversation_id); setView("chat")
      await refreshRef.current()
    }),
    createTask: (input: { taskId: string; title: string; goal: string; acceptance: string }) => withMutation(`create-task:${activeId.current}`, async () => {
      const context = requireContext(), id = context.conversation?.conversation_id
      if (!id) throw new Error("先选择会话。")
      const title = inputText(input.title, "工单标题", 65536)
      const goal = inputText(input.goal, "工单目标", 65536)
      const acceptance = inputText(input.acceptance, "验收条件", 65536)
      const prior = tasksRef.current.find((item) => item.conversation_id === id)
      if (prior && (prior.title !== title || prior.goal !== goal || prior.acceptance !== acceptance)) throw new Error("还有待确认的工单，请保留原内容重试。")
      const request = prior ?? { conversation_id: id, task_id: input.taskId, title, goal, acceptance }
      const next = [...tasksRef.current.filter((item) => item.conversation_id !== id), request]
      writeStorage(scoped("pending-tasks"), next, true)
      tasksRef.current = next; setPendingTasks(next)
      await connection.rpc("create_task", request)
      requireCurrent()
      const settled = tasksRef.current.filter((item) => item.task_id !== request.task_id)
      writeStorage(scoped("pending-tasks"), settled, true)
      tasksRef.current = settled; setPendingTasks(settled)
      if (activeId.current === id) await refreshRef.current()
    }),
    updateTask: (id: string, taskId: string, status: string, detail: string) => withMutation(`update-task:${taskId}`, async () => {
      requireContext(id)
      await connection.rpc("update_task", { conversation_id: id, task_id: taskId, status, detail })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    sendMessage: (text: string, reportResults = false) => withMutation(`message:${activeId.current}`, async () => {
      const context = requireContext(), id = activeId.current
      const prior = messagesRef.current.find((item) => item.conversation_id === id) ?? null
      const scheduler = nodes.find((item) => item.node_id === (prior?.scheduler_node ?? schedulerNodeId))
      if (!canSelectScheduler(scheduler)) throw new Error("原调度设备的授权或已知管理配置不可用，请先核对原消息。")
      const prepared = prepareSend({ conversationId: id, text, schedulerNodeId: scheduler.node_id, entryNode: connection.entry.node_id, pending: prior, heads: context.heads, reportResults })
      if (!prepared.ok) throw new Error(prepared.error)
      replaceMessages([...messagesRef.current.filter((item) => item.conversation_id !== id), prepared.pending])
      setMessageBusy(true)
      try {
        const receipt = await connection.rpc("message", prepared.body)
        requireCurrent()
        if (!isQueuedReceipt(receipt, prepared.pending)) {
          setNoticeText("入口已接收请求，尚未确认目标调度端入队。保留原消息核对，不会自动重发。")
          if (activeId.current === id) await refreshRef.current().catch(showError)
          return
        }
        replaceMessages(messagesRef.current.filter((item) => item.message_id !== prepared.pending.message_id))
        setNoticeText("消息已可靠入队，等待调度 Agent 处理。")
        if (activeId.current === id) await refreshRef.current().catch(showError)
      } catch (caught) {
        if (current() && caught instanceof Error && "responded" in caught && caught.responded === true) {
          const original = messagesRef.current.find((item) => item.message_id === prepared.pending.message_id)
          if (original) replaceMessages(messagesRef.current.map((item) => item === original ? { ...item, rejected: { entry_node: connection.entry.node_id } } : item))
        }
        throw caught
      } finally { if (current()) setMessageBusy(false) }
    }),
    cancelManager: (trigger: string) => withMutation(`cancel-manager:${trigger}`, async () => {
      const id = activeId.current
      requireContext(id)
      await connection.rpc("cancel_manager", { conversation_id: id, trigger })
      requireCurrent()
      setNoticeText("已提交停止回复请求。已派出的后台任务会单独继续，最终状态以核对结果为准。")
      if (activeId.current === id) await refreshRef.current()
    }),
    setFollowup: (taskId: string, mode: FollowupMode) => withMutation(`followup:${taskId}`, async () => {
      const id = activeId.current
      const context = requireContext(id), task = context.tasks.find((item) => item.task_id === taskId)
      if (task?.followup_version === undefined) throw new Error("当前入口尚未提供可核对的授权版本，请先更新节点并刷新。")
      try { await connection.rpc("set_followup", { conversation_id: id, task_id: taskId, mode, expected_version: task.followup_version, scheduler_node: task.followup_scheduler_node ?? schedulerNodeId }) }
      catch (caught) { if (current() && activeId.current === id) await refreshRef.current(); throw caught }
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    cancelFollowup: (task: TaskRecord) => withMutation(`followup:${task.task_id}`, async () => {
      const id = activeId.current
      requireContext(id)
      if (task.followup_version === undefined) throw new Error("当前入口尚未提供可核对的授权版本，请先更新节点并刷新。")
      try { await connection.rpc("set_followup", { conversation_id: id, task_id: task.task_id, mode: "off", expected_version: task.followup_version, scheduler_node: task.followup_scheduler_node ?? schedulerNodeId }) }
      catch (caught) { if (current() && activeId.current === id) await refreshRef.current(); throw caught }
      requireCurrent()
      await connection.rpc("update_task", { conversation_id: id, task_id: task.task_id, status: "cancelled", detail: task.detail })
      requireCurrent()
      setNoticeText("已取消尚未开始的后续安排；当前执行是否停止需单独核对。")
      if (activeId.current === id) await refreshRef.current()
    }),
    taskControl: async (taskId: string, action: "status" | "interrupt" | "steer" | "respond_request", fields: { text?: string; request_id?: string | number; response?: unknown } = {}) => {
      const id = activeId.current
      if (action === "status") {
        requireCurrent()
        await connection.rpc("task_control", { conversation_id: id, task_id: taskId, action })
        requireCurrent()
        if (activeId.current === id) await refreshRef.current()
        return
      }
      await withMutation(`task-control:${taskId}`, async () => {
        const context = requireContext(id), task = context.tasks.find((item) => item.task_id === taskId), binding = task?.binding
        if (!binding?.operation_id) throw new Error("缺少当前后台操作身份，请先核对执行状态。")
        if (pendingRef.current.some((item) => item.target_node === binding.target_node && item.session_id === binding.session_id)) throw new Error("此执行有原操作待核对，请勿生成另一请求。")
        const request: TaskControlRequest = { operation_id: newId(), network_id: networkId, conversation_id: id, task_id: taskId, target_node: binding.target_node, session_id: binding.session_id, expected_operation_id: binding.operation_id, expected_revision: binding.revision, action, ...fields }
        replacePending([...pendingRef.current, { operation_id: request.operation_id, conversation_id: id, target_node: request.target_node, session_id: request.session_id, control: request }])
        await submitTaskControl(request)
      })
    },
    retryTaskControl: (operation: PendingOperation) => withMutation(`task-control:${operation.control?.task_id ?? operation.operation_id}`, async () => {
      requireContext(operation.conversation_id)
      const original = pendingRef.current.find((item) => item.operation_id === operation.operation_id)
      if (!original?.control) throw new Error("原控制请求不存在，请先核对持久状态。")
      await submitTaskControl(original.control)
    }),
    resolveManagerUnknown: (trigger: string, evidence: string) => withMutation(`resolve-manager:${trigger}`, async () => {
      const id = activeId.current
      requireContext(id)
      if (new TextEncoder().encode(evidence.trim()).length < 12) throw new Error("请填写至少 12 字节的本人核查证据。")
      await connection.rpc("resolve_manager_unknown", { conversation_id: id, trigger, evidence: evidence.trim() })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    readArtifact: async (hash: string, offset = 0) => {
      const id = activeId.current
      requireCurrent()
      const result = parseArtifactPage(await connection.rpc("read_artifact", { conversation_id: id, blob_hash: hash, offset, limit: 65536 }), hash, offset)
      requireCurrent()
      if (activeId.current !== id) throw new Error("会话已切换，请在原会话中重新打开产物。")
      return result
    },
    releaseMessagePending: () => withMutation(`message:${activeId.current}`, async () => {
      requireCurrent()
      const id = activeId.current, original = messagesRef.current.find((item) => item.conversation_id === id)
      if (!original) throw new Error("原消息请求尚未结束，请先核对。")
      if (original.entry_node !== connection.entry.node_id || original.rejected?.entry_node !== connection.entry.node_id) throw new Error("需要原入口明确返回错误，再在同一连接核对完整会话。超时或断线后不能据此解除，请原样重试并核对。")
      const complete = await readConversation(id)
      requireContext(id)
      if (messagesRef.current.find((item) => item.conversation_id === id) !== original || complete.messages.some((item) => item.message_id === original.message_id)) throw new Error("已找到原消息或待核对记录变化，请保留原记录并核对调度结果。")
      replaceMessages(messagesRef.current.filter((item) => item !== original))
      setNoticeText("已解除本地待核对记录，未重发消息。")
    }),
    operate: (target: { target_node: string; session_id: string; revision?: number }, action: OperateAction, id: string) => withMutation(`operate:${target.target_node}:${target.session_id}`, async () => {
      const context = requireContext(id)
      if (pendingRef.current.some((item) => item.conversation_id === id && item.target_node === target.target_node && (action.type === "open" || item.session_id === target.session_id))) throw new Error("此设备有尚未确认的操作，请先核对原操作。")
      const session = context.sessions.find((item) => item.target_node === target.target_node && item.session_id === target.session_id)
      const task = context.tasks.find((item) => item.binding?.target_node === target.target_node && item.binding.session_id === target.session_id)
      if (session?.state === "unknown" && action.type !== "read") throw new Error("执行结果未知，请先读取实际状态并人工核对。")
      const request: OperationRequest = { operation_id: newId(), network_id: networkId, conversation_id: id, task_id: task?.task_id ?? null, target_node: target.target_node, session_id: target.session_id, expected_revision: target.revision ?? 0, basis_heads: context.heads, resources: [], action }
      replacePending([...pendingRef.current, { operation_id: request.operation_id, target_node: request.target_node, session_id: request.session_id, conversation_id: id, request }])
      const result = parseOperation(record(await connection.rpc("operate", { request })).operation)
      requireCurrent()
      if (result.request.operation_id !== request.operation_id || result.request.target_node !== target.target_node || result.request.conversation_id !== id || result.request.session_id !== target.session_id) throw new Error("响应操作与原请求不一致，请核对原操作。")
      removePending(request.operation_id)
      setNoticeText(`操作已提交（${stateNames[result.state] ?? result.state}）。`)
      if (activeId.current === id) await refreshRef.current()
    }),
    reconcile: async (operation: PendingOperation): Promise<ReconcileRecord> => {
      requireCurrent()
      const result = parseReconcile(await connection.rpc("reconcile", { target_node: operation.target_node, operation_id: operation.operation_id }))
      requireCurrent()
      if (reconcileDecision(result, operation) === "settled" && writer.current) {
        removePending(operation.operation_id)
        if (activeId.current === operation.conversation_id) await refreshRef.current()
      }
      return result
    },
    confirmMissing: async (operationId: string) => {
      requireCurrent()
      const operation = pendingRef.current.find((item) => item.operation_id === operationId)
      if (!operation || operation.conversation_id !== activeId.current) throw new Error("请先回到原操作的会话。")
      return withMutation(`operate:${operation.target_node}:${operation.session_id}`, async () => {
        if (connection.entry.node_id !== operation.target_node) throw new Error("请连接原执行设备，再读取完整会话并核对原操作。")
        const result = parseReconcile(await connection.rpc("reconcile", { target_node: operation.target_node, operation_id: operation.operation_id }))
        requireCurrent()
        if (reconcileDecision(result, operation) !== "missing") throw new Error("原执行设备没有确认该操作缺失，已保留待核对记录。")
        const complete = await readConversation(operation.conversation_id)
        requireContext(operation.conversation_id)
        if (complete.operations.some((item) => item.request.operation_id === operationId)) throw new Error("原操作已有执行记录，不能解除。")
        removePending(operationId)
        setNoticeText("已解除本地待核对记录，未重发操作。")
      })
    },
    readSession: async (item: SessionRecord) => {
      const id = activeId.current
      requireCurrent()
      const result = record(await connection.rpc("session", { target_node: item.target_node, session_id: item.session_id }))
      const parsed = parseSession(result.session)
      requireCurrent()
      if (activeId.current !== id) return
      if (parsed.target_node !== item.target_node || parsed.session_id !== item.session_id) throw new Error("执行设备返回了其他会话。")
      setActual((previous) => ({ ...previous, [`${item.target_node}:${item.session_id}`]: result.actual }))
    },
    clearActual: (item: SessionRecord) => { setActual((previous) => { const next = { ...previous }; delete next[`${item.target_node}:${item.session_id}`]; return next }) },
    resolveUnknown: (targetNode: string, operationId: string, outcome: string, evidence: string) => withMutation(`resolve:${operationId}`, async () => {
      const id = activeId.current
      requireContext(id)
      if (evidence.trim().length < 12 || !["succeeded", "failed"].includes(outcome)) throw new Error("请填写本人核查证据与明确结果。")
      await connection.rpc("resolve_unknown", { target_node: targetNode, operation_id: operationId, outcome, evidence })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    ack: async (notificationId: string) => { requireWriter(); await connection.rpc("ack", { notification_id: notificationId }); requireCurrent(); await refreshRef.current() },
    openNotification: async (notification: NotificationRecord) => { selectConversation(notification.conversation_id); setView("chat"); await refreshRef.current() },
    enableBrowserNotifications: async () => {
      requireCurrent()
      if (!("Notification" in window)) return
      const permission = Notification.permission === "granted" ? "granted" : await Notification.requestPermission()
      requireCurrent()
      writeStorage(networkKey("notify", networkId), permission === "granted")
      if (permission === "granted") { writeStorage(networkKey("seen", networkId), notificationsRef.current.map((item) => item.notification_id)); setNoticeText("浏览器提醒已开启。已有通知保留在通知列表中。") }
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
