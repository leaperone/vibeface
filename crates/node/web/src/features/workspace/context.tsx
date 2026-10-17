"use client"

import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react"
import { logout, openEvents, rpc, type Session } from "@/lib/node"
import { conversationHref, entryHref, validEntry } from "./entry-url"
import { loadInfo } from "./info"
import { isIncomplete, mergeBasis, missingCount, stateNames } from "./labels"
import {
  frontierMap,
  parseConversation,
  parseConversations,
  parseNodes,
  parseNotifications,
  parsePending,
} from "./model"
import { publishNotice } from "./notice"
import { errorText, reconcileDecision, shouldDropPending, statusOf, type ReconcileRecord } from "./reconcile"
import { ensureClientId, networkKey, newId, readStorage, writeStorage } from "./storage"
import type {
  ChatSlotState,
  ConversationPayload,
  ConversationSummary,
  EntryInfo,
  NodeInfo,
  NotificationRecord,
  OperateAction,
  PendingOperation,
  SessionRecord,
  WorkspaceView,
} from "./types"

type WorkspaceContextValue = {
  session: Session
  info: EntryInfo
  connectionText: string
  connectionOnline: boolean
  error: string
  noticeText: string
  view: WorkspaceView
  setView: (view: WorkspaceView) => void
  nodes: NodeInfo[]
  conversations: ConversationSummary[]
  conversationId: string
  data: ConversationPayload | null
  notifications: NotificationRecord[]
  pending: PendingOperation[]
  actual: Record<string, unknown>
  refreshing: boolean
  schedulerNodeId: string
  setSchedulerNodeId: (nodeId: string) => void
  syncText: string
  incomplete: boolean
  chatSlot: ChatSlotState
  entries: string[]
  reloadEntries: () => void
  saveEntry: (address: string) => void
  removeEntry: (origin: string) => void
  hrefForEntry: (origin: string) => string
  selectConversation: (conversationId: string) => void
  refresh: () => Promise<void>
  run: (action: () => Promise<void>) => Promise<void>
  createConversation: (conversationId: string, title: string) => Promise<void>
  createTask: (input: {
    taskId: string
    title: string
    goal: string
    acceptance: string
    autoFollowup: boolean
  }) => Promise<void>
  updateTask: (conversationId: string, taskId: string, status: string, detail: string) => Promise<void>
  operate: (
    session: { target_node: string; session_id: string; revision?: number },
    action: OperateAction,
    conversationId: string,
  ) => Promise<void>
  reconcile: (operation: PendingOperation) => Promise<ReconcileRecord>
  confirmMissing: (operationId: string) => void
  readSession: (session: SessionRecord) => Promise<void>
  clearActual: (session: SessionRecord) => void
  resolveUnknown: (targetNode: string, operationId: string, outcome: string, evidence: string) => Promise<void>
  revoke: (nodeId: string) => Promise<void>
  ack: (notificationId: string) => Promise<void>
  invite: (roles: string[]) => Promise<string>
  openNotification: (notification: NotificationRecord) => Promise<void>
  enableBrowserNotifications: () => Promise<void>
  notificationButton: { label: string; disabled: boolean }
  announce: (text: string) => void
  logout: () => Promise<void>
}

const WorkspaceContext = createContext<WorkspaceContextValue | null>(null)

export function useWorkspace() {
  const value = useContext(WorkspaceContext)
  if (!value) throw new Error("工作空间屏幕需要放在 WorkspaceShell 中。")
  return value
}

function readFrontiers(networkId: string) {
  return frontierMap(readStorage(networkKey("frontiers", networkId), {}))
}

function writeFrontiers(networkId: string, frontiers: Record<string, string[]>) {
  writeStorage(networkKey("frontiers", networkId), frontiers)
}

export function WorkspaceProvider({
  session,
  onLogout,
  children,
}: {
  session: Session
  onLogout: () => void
  children: ReactNode
}) {
  const networkId = session.account.network_id
  const epochRef = useRef(0)
  const conversationIdRef = useRef("")
  const dataRef = useRef<ConversationPayload | null>(null)
  const pendingRef = useRef<PendingOperation[]>([])
  const notificationsRef = useRef<NotificationRecord[]>([])
  const refreshRef = useRef<() => Promise<void>>(async () => {})
  const refreshingRef = useRef(false)
  const refreshAgainRef = useRef(false)
  const refreshTimerRef = useRef<number | null>(null)
  const liveRef = useRef(false)
  const [clientId, setClientId] = useState("")
  const [booted, setBooted] = useState(false)
  const [info, setInfo] = useState<EntryInfo>({})
  const [connectionOnline, setConnectionOnline] = useState(true)
  const [connectionText, setConnectionText] = useState("入口可达")
  const [error, setError] = useState("")
  const [noticeText, setNoticeText] = useState("")
  const [view, setView] = useState<WorkspaceView>("chat")
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [conversations, setConversations] = useState<ConversationSummary[]>([])
  const [conversationId, setConversationId] = useState("")
  const [data, setData] = useState<ConversationPayload | null>(null)
  const [notifications, setNotifications] = useState<NotificationRecord[]>([])
  const [pending, setPending] = useState<PendingOperation[]>([])
  const [actual, setActual] = useState<Record<string, unknown>>({})
  const [refreshing, setRefreshing] = useState(false)
  const [schedulerNodeId, setSchedulerNodeId] = useState("")
  const [entries, setEntries] = useState<string[]>([])
  const [notificationButton, setNotificationButton] = useState({ label: "开启浏览器提醒", disabled: false })

  const showError = (caught: unknown) => setError(errorText(caught))

  const replacePending = (next: PendingOperation[], required = false) => {
    writeStorage(networkKey("pending", networkId), next, required)
    pendingRef.current = next
    setPending(next)
  }

  const removePending = (operationId: string) => {
    replacePending(pendingRef.current.filter((item) => item.operation_id !== operationId))
  }

  const selectConversation = (nextId: string) => {
    conversationIdRef.current = nextId
    setConversationId(nextId)
    dataRef.current = null
    setData(null)
    if (typeof window === "undefined") return
    const href = conversationHref(window.location.href, nextId)
    window.history.replaceState(null, "", href)
  }

  const rememberHeads = (id: string, heads: string[]) => {
    const frontiers = readFrontiers(networkId)
    frontiers[id] = heads
    writeFrontiers(networkId, frontiers)
  }

  const notifyBrowser = (items: NotificationRecord[]) => {
    if (typeof window === "undefined" || !("Notification" in window) || Notification.permission !== "granted") return
    if (!readStorage(networkKey("notify", networkId), false)) return
    const seen = new Set(readStorage<string[]>(networkKey("seen", networkId), []))
    for (const item of items) {
      if (seen.has(item.notification_id)) continue
      seen.add(item.notification_id)
      try {
        const popup = new Notification("Vibeface · 任务进展", { body: item.text, tag: item.notification_id })
        popup.onclick = () => {
          window.focus()
          selectConversation(item.conversation_id)
          setView("chat")
          void refreshRef.current().catch(showError)
          popup.close()
        }
      } catch {
        // The in-page inbox remains available on unsupported browsers.
      }
    }
    writeStorage(networkKey("seen", networkId), [...seen].slice(-1000))
  }

  const refreshNotificationButton = () => {
    if (typeof window === "undefined" || !("Notification" in window)) {
      setNotificationButton({ label: "浏览器不支持提醒", disabled: true })
      return
    }
    const enabled = readStorage(networkKey("notify", networkId), false)
    if (Notification.permission === "granted") {
      setNotificationButton({ label: enabled ? "浏览器提醒已开启" : "启用此网络提醒", disabled: enabled })
      return
    }
    if (Notification.permission === "denied") {
      setNotificationButton({ label: "请在浏览器设置允许提醒", disabled: true })
      return
    }
    setNotificationButton({ label: "开启浏览器提醒", disabled: false })
  }

  const refresh = async () => {
    if (refreshingRef.current) {
      refreshAgainRef.current = true
      return
    }
    const epoch = epochRef.current
    refreshingRef.current = true
    setRefreshing(true)
    try {
      const [nodesResult, conversationsResult, notificationsResult] = await Promise.all([
        rpc("nodes", {}),
        rpc("conversations", {}),
        rpc("notifications", { client_id: clientId }),
      ])
      if (epoch !== epochRef.current) return
      const nextNodes = parseNodes(nodesResult)
      const nextConversations = parseConversations(conversationsResult)
      const nextNotifications = parseNotifications(notificationsResult)
      if (!conversationIdRef.current && nextConversations[0]) {
        conversationIdRef.current = nextConversations[0].conversation_id
        setConversationId(nextConversations[0].conversation_id)
      }
      let nextData: ConversationPayload | null = null
      const activeId = conversationIdRef.current
      if (activeId) {
        const payload = parseConversation(await rpc("conversation", {
          conversation_id: activeId,
          basis_heads: readFrontiers(networkId)[activeId] || [],
        }))
        if (epoch !== epochRef.current) return
        if (activeId === conversationIdRef.current) {
          nextData = payload
          if (!isIncomplete(payload.missing)) rememberHeads(activeId, payload.heads || [])
        }
      }
      if (epoch !== epochRef.current) return
      setNodes(nextNodes)
      setConversations(nextConversations)
      setNotifications(nextNotifications)
      notificationsRef.current = nextNotifications
      dataRef.current = nextData
      setData(nextData)
      setSchedulerNodeId((current) => {
        if (typeof document !== "undefined" && document.activeElement?.id === "scheduler-node") return current
        const choices = nextNodes.filter((node) => node.roles?.includes("scheduler") && node.capabilities?.manager)
        if (choices.some((node) => node.node_id === current)) return current
        return choices[0]?.node_id ?? ""
      })
      notifyBrowser(nextNotifications)
      refreshNotificationButton()
    } finally {
      refreshingRef.current = false
      setRefreshing(false)
      if (refreshAgainRef.current && epoch === epochRef.current) {
        refreshAgainRef.current = false
        scheduleRefresh()
      }
    }
  }
  refreshRef.current = refresh

  const scheduleRefresh = () => {
    if (refreshTimerRef.current !== null) window.clearTimeout(refreshTimerRef.current)
    refreshTimerRef.current = window.setTimeout(() => {
      refreshTimerRef.current = null
      void refreshRef.current().catch(showError)
    }, 350)
  }

  useEffect(() => {
    const id = ensureClientId()
    setClientId(id)
    const params = new URL(window.location.href).searchParams
    const fromUrl = params.get("conversation") || ""
    if (fromUrl) {
      conversationIdRef.current = fromUrl
      setConversationId(fromUrl)
    }
    const storedPending = parsePending(readStorage(networkKey("pending", networkId), []))
    pendingRef.current = storedPending
    setPending(storedPending)
    const frontiers = readFrontiers(networkId)
    const currentHeads = frontiers[conversationIdRef.current] || []
    const merged = mergeBasis(currentHeads, params.get("basis"))
    if (merged !== currentHeads) {
      frontiers[conversationIdRef.current] = [...merged]
      writeFrontiers(networkId, frontiers)
    }
    const known = readStorage<string[]>("entries", []).filter((value) => {
      try {
        return validEntry(value) === value
      } catch {
        return false
      }
    })
    setEntries([...new Set([window.location.origin, ...known])])
    setBooted(true)
  }, [networkId])

  useEffect(() => {
    if (!booted || !clientId) return
    const epoch = epochRef.current
    void refreshRef.current().catch(showError)
    const close = openEvents(clientId, () => {
      liveRef.current = true
      setConnectionOnline(true)
      setConnectionText("实时连接已建立")
      scheduleRefresh()
    })
    const timer = window.setInterval(() => { void refreshRef.current().catch(showError) }, 15000)
    const onOnline = () => scheduleRefresh()
    const onVisible = () => {
      if (!document.hidden) scheduleRefresh()
    }
    window.addEventListener("online", onOnline)
    document.addEventListener("visibilitychange", onVisible)
    return () => {
      if (epoch === epochRef.current) epochRef.current += 1
      if (typeof close === "function") close()
      if (refreshTimerRef.current !== null) window.clearTimeout(refreshTimerRef.current)
      window.clearInterval(timer)
      window.removeEventListener("online", onOnline)
      document.removeEventListener("visibilitychange", onVisible)
    }
  }, [booted, clientId])

  useEffect(() => {
    void loadInfo().then((next) => {
      setInfo(next)
      if (liveRef.current) return
      setConnectionOnline(true)
      setConnectionText("入口可达")
    }).catch((caught: unknown) => {
      if (liveRef.current) return
      setConnectionOnline(false)
      setConnectionText("入口连接失败")
      showError(caught)
    })
  }, [])

  const run = async (action: () => Promise<void>) => {
    setError("")
    try {
      await action()
    } catch (caught) {
      showError(caught)
    }
  }

  const reloadEntries = () => {
    const known = readStorage<string[]>("entries", []).filter((value) => {
      try {
        return validEntry(value) === value
      } catch {
        return false
      }
    })
    setEntries([...new Set([window.location.origin, ...known])])
  }

  const incomplete = isIncomplete(data?.missing)
  const scheduler = nodes.find((node) => node.node_id === schedulerNodeId)
  const chatSlot: ChatSlotState = {
    conversationId: conversationId || null,
    schedulerNodeId: schedulerNodeId || null,
    missing: !data || incomplete,
    schedulerOnline: Boolean(scheduler?.online),
    managerReady: Boolean(scheduler?.capabilities?.manager),
  }
  const syncText = !data
    ? "选择或创建会话后，可发消息和派发任务。"
    : incomplete
      ? `上下文尚未完整，已暂停派发。缺少事件 ${missingCount(data.missing?.events)}，附件 ${missingCount(data.missing?.blobs)}，冲突 ${missingCount(data.missing?.conflicts)}。`
      : `此入口已取得已知上下文 · ${(data.heads || []).length} 个历史位置。其他离线设备可能仍有未同步记录。`

  const value = useMemo<WorkspaceContextValue>(() => ({
    session,
    info,
    connectionText,
    connectionOnline,
    error,
    noticeText,
    view,
    setView,
    nodes,
    conversations,
    conversationId,
    data,
    notifications,
    pending,
    actual,
    refreshing,
    schedulerNodeId,
    setSchedulerNodeId,
    syncText,
    incomplete,
    chatSlot,
    entries,
    reloadEntries,
    saveEntry: (address) => {
      const origin = validEntry(address.trim())
      writeStorage("entries", [...new Set([...readStorage<string[]>("entries", []), origin])])
      reloadEntries()
      setNoticeText("入口地址已保存。切换入口后需要在该地址重新登录。")
    },
    removeEntry: (origin) => {
      writeStorage("entries", readStorage<string[]>("entries", []).filter((entry) => entry !== origin))
      reloadEntries()
    },
    hrefForEntry: (origin) => entryHref(origin, conversationIdRef.current, readFrontiers(networkId)[conversationIdRef.current] || []),
    selectConversation,
    refresh,
    run,
    createConversation: async (id, title) => {
      await rpc("create_conversation", { conversation_id: id, title })
      selectConversation(id)
      setView("chat")
      await refreshRef.current()
      document.querySelector<HTMLTextAreaElement>('[data-slot="chat"] textarea')?.focus()
    },
    createTask: async (input) => {
      if (!dataRef.current || isIncomplete(dataRef.current.missing)) throw new Error("先选择上下文完整的会话。")
      await rpc("create_task", {
        conversation_id: conversationIdRef.current,
        task_id: input.taskId,
        title: input.title,
        goal: input.goal,
        acceptance: input.acceptance,
        auto_followup: input.autoFollowup,
      })
      await refreshRef.current()
      setNoticeText("工单已创建。")
    },
    updateTask: async (ownerId, taskId, status, detail) => {
      if (ownerId !== conversationIdRef.current) throw new Error("会话已切换，请在当前工单上重新操作。")
      await rpc("update_task", { conversation_id: ownerId, task_id: taskId, status, detail })
      if (document.activeElement instanceof HTMLElement) document.activeElement.blur()
      await refreshRef.current()
      setNoticeText("工单状态已保存。")
    },
    operate: async (target, action, ownerId) => {
      if (ownerId !== conversationIdRef.current) throw new Error("会话已切换，请在当前执行会话上重新操作。")
      if (!dataRef.current || isIncomplete(dataRef.current.missing)) throw new Error("先补齐当前会话上下文。")
      if (pendingRef.current.some((item) => item.conversation_id === conversationIdRef.current && item.target_node === target.target_node && (action.type === "open" || item.session_id === target.session_id))) {
        throw new Error("此设备有尚未确认的操作，请先核对原操作，再提交新指令。")
      }
      const current = dataRef.current.sessions?.find((item) => item.target_node === target.target_node && item.session_id === target.session_id)
      if (current?.state === "unknown" && action.type !== "read") {
        throw new Error("执行会话结果未知，请先读取实际状态并完成核对。")
      }
      const operationId = newId()
      const request = {
        operation_id: operationId,
        network_id: networkId,
        conversation_id: conversationIdRef.current,
        task_id: null,
        target_node: target.target_node,
        session_id: target.session_id,
        expected_revision: target.revision ?? 0,
        basis_heads: dataRef.current.heads || [],
        resources: [] as unknown[],
        action,
      }
      const nextPending = [...pendingRef.current, {
        operation_id: operationId,
        target_node: target.target_node,
        session_id: target.session_id,
        conversation_id: conversationIdRef.current,
      }]
      replacePending(nextPending, true)
      const epoch = epochRef.current
      try {
        const result: unknown = await rpc("operate", { request })
        if (epoch !== epochRef.current) return
        removePending(operationId)
        const state = result && typeof result === "object" ? (result as { state?: unknown }).state : undefined
        const label = typeof state === "string" ? stateNames[state] || "等待执行结果" : "等待执行结果"
        setNoticeText(`操作已提交（${label}）。`)
        await refreshRef.current()
      } catch (caught) {
        if (epoch === epochRef.current && shouldDropPending(statusOf(caught))) removePending(operationId)
        throw caught
      }
    },
    reconcile: async (operation) => {
      const raw: unknown = await rpc("reconcile", { target_node: operation.target_node, operation_id: operation.operation_id })
      const result = (raw && typeof raw === "object" ? raw : {}) as ReconcileRecord
      const decision = reconcileDecision(result, operation)
      if (decision === "settled") {
        removePending(operation.operation_id)
        await refreshRef.current()
      } else if (decision === "pending") {
        const stateValue = result.state || result.operation?.state
        setNoticeText(`原操作状态：${stateNames[stateValue || ""] || "暂未找到，请保持待核对"}。`)
      }
      return result
    },
    confirmMissing: (operationId) => {
      removePending(operationId)
      setNoticeText("已解除本地待核对记录，未重发操作。")
    },
    readSession: async (item) => {
      const result: unknown = await rpc("session", { target_node: item.target_node, session_id: item.session_id })
      const body = result && typeof result === "object" ? (result as { actual?: unknown }) : {}
      if (body.actual !== undefined) {
        setActual((current) => ({ ...current, [`${item.target_node}:${item.session_id}`]: body.actual }))
      }
      setNoticeText("已从执行设备读取状态。")
    },
    clearActual: (item) => {
      setActual((current) => {
        const next = { ...current }
        delete next[`${item.target_node}:${item.session_id}`]
        return next
      })
    },
    resolveUnknown: async (targetNode, operationId, outcome, evidence) => {
      await rpc("resolve_unknown", { target_node: targetNode, operation_id: operationId, outcome, evidence })
      await refreshRef.current()
    },
    revoke: async (nodeId) => {
      await rpc("revoke", { node_id: nodeId })
      await refreshRef.current()
      setNoticeText("设备授权已撤销。")
    },
    ack: async (notificationId) => {
      await rpc("ack", { notification_id: notificationId, client_id: clientId })
      await refreshRef.current()
    },
    invite: async (roles) => {
      if (!roles.length) throw new Error("至少选择一个设备角色。")
      const result: unknown = await rpc("invite", { roles })
      const body = result && typeof result === "object" ? (result as { command?: unknown; code?: unknown }) : {}
      if (typeof body.command === "string" && body.command) return body.command
      if (typeof body.code === "string") return body.code
      return ""
    },
    openNotification: async (notification) => {
      selectConversation(notification.conversation_id)
      setView(notification.kind === "approval" || notification.kind === "needs_input" ? "sessions" : "chat")
      await refreshRef.current()
    },
    enableBrowserNotifications: async () => {
      if (!("Notification" in window)) return
      const permission = Notification.permission === "granted" ? "granted" : await Notification.requestPermission()
      writeStorage(networkKey("notify", networkId), permission === "granted")
      if (permission === "granted") {
        writeStorage(networkKey("seen", networkId), notificationsRef.current.map((item) => item.notification_id))
        setNoticeText("浏览器提醒已开启。已有通知保留在通知列表中。")
      }
      refreshNotificationButton()
    },
    notificationButton,
    announce: (text) => setNoticeText(text),
    logout: async () => {
      await logout()
      epochRef.current += 1
      publishNotice("已退出此入口。")
      onLogout()
    },
  }), [
    session,
    info,
    connectionText,
    connectionOnline,
    error,
    noticeText,
    view,
    nodes,
    conversations,
    conversationId,
    data,
    notifications,
    pending,
    actual,
    refreshing,
    schedulerNodeId,
    incomplete,
    chatSlot,
    entries,
    notificationButton,
    networkId,
    clientId,
    onLogout,
  ])

  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>
}
