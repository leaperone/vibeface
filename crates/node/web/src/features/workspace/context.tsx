"use client"

import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import type { ReadyConnection, RpcFacts } from "@/lib/node"
import { integer, same, text } from "@/lib/node"
import { parseRemoteExecutionShare, parseTrustedRemoteExecution, remoteExecutionParams, remoteExecutionProposal } from "@/lib/remote-execution"
import type { RemoteExecutionObservation, RemoteExecutionPurpose, RemoteExecutionSession, TrustedRemoteExecution } from "@/lib/remote-execution"
import { prepareContext } from "@/lib/rpc-prepare"
import type { RemoteExecutionPolicyUse } from "@/lib/remote-execution-policy-ingress"
import { nodeFingerprint, signedInvocationHash, verifyRemoteExecutionSeal } from "../../../generated/vibeface_secure.js"
import { verifyBoundExecutionSelection, verifyExecutionBinding, verifyTaskCreation } from "@/lib/task-proof"
import { prepareSend } from "../chat/protocol"
import { conversationHref } from "./entry-url"
import { isIncomplete, mergeBasis, missingCount, stateNames } from "./labels"
import {
  frontierMap, inputText, parseAction, parseConversation, parseConversations, parseNodes, parseNotifications, parseOperation, parseOperationRequest,
  parsePending, parsePendingMessages, parsePendingConversation, parsePendingTasks, parsePendingBindings, parsePendingRemoteResolutions, parseSession, parseTask, record,
} from "./model"
import { errorText, parseReconcile, reconcileDecision, type ReconcileRecord } from "./reconcile"
import { projectScopeKey } from "@/lib/scope"
import { newId, readStorage, writeStorage } from "./storage"
import { acceptRebindSeal, acceptReboundBinding, parsePendingRebinds, prepareRemoteRebind, recordRebindAttempt, remoteSealStatusBody } from "@/lib/remote-rebind"
import type { PendingRebind } from "@/lib/remote-rebind"
import type {
  ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, OperateAction,
  OperationRequest, PendingOperation, PendingMessage, PendingConversation, PendingTask, PendingBinding, PendingRemoteResolution, SessionRecord, TaskRecord, WorkspaceView,
} from "./types"

function useWorkspaceState(connection: ReadyConnection) {
  const spaceId = connection.scope.space_id
  const canAutoFollowup = connection.info.roles.includes("gateway")
  const canReadNotifications = connection.authorization.actions.includes("notification_read")
  const canWrite = connection.authorization.actions.some(action => ["conversation_create", "message_append", "task_create", "task_progress", "task_accept", "task_cancel", "execution_open", "execution_send", "execution_steer", "execution_interrupt", "execution_respond_approval", "execution_rebind", "operation_resolve", "notification_ack"].includes(action))
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
  const [pendingBindings, setPendingBindings] = useState<PendingBinding[]>([])
  const [remoteExecutions, setRemoteExecutions] = useState<TrustedRemoteExecution[]>([])
  const [remoteObservations, setRemoteObservations] = useState<Record<string, RemoteExecutionObservation>>({})
  const [pendingRemoteResolutions, setPendingRemoteResolutions] = useState<PendingRemoteResolution[]>([])
  const [pendingRemoteRebinds, setPendingRemoteRebinds] = useState<PendingRebind[]>([])
  const [actual, setActual] = useState<Record<string, unknown>>({})
  const [refreshing, setRefreshing] = useState(false)
  const [messageBusy, setMessageBusy] = useState(false)
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
  const bindingsRef = useRef<PendingBinding[]>([])
  const remotesRef = useRef<TrustedRemoteExecution[]>([])
  const remoteSessions = useRef(new Map<string, RemoteExecutionSession>())
  const observationsRef = useRef(new Map<string, RemoteExecutionObservation>())
  const resolutionsRef = useRef<PendingRemoteResolution[]>([])
  const rebindsRef = useRef<PendingRebind[]>([])
  const notificationsRef = useRef<NotificationRecord[]>([])
  const popups = useRef(new Set<Notification>())
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
  const requireStorageWriter = () => {
    requireCurrent()
    if (!writer.current) throw new Error("此页面只读。关闭正在操作此设备的其他页面，再重新连接。浏览器需支持 Web Locks。")
  }
  const requireWriter = () => {
    requireStorageWriter()
    if (!canWrite) throw new Error("当前项目授权仅供读取历史，不能写入、派发或修改工单。请切换到具有相应写权限的项目。")
  }
  const scoped = (key: string) => projectScopeKey(connection.scope, connection.project.id, key)
  const readFrontiers = () => frontierMap(readStorage<unknown>(scoped("frontiers"), {}, true))
  const replacePending = (next: PendingOperation[]) => {
    requireStorageWriter()
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
  const replaceBindings = (next: PendingBinding[]) => {
    requireStorageWriter()
    writeStorage(scoped("pending-bindings"), next, true)
    bindingsRef.current = next
    if (current()) setPendingBindings(next)
  }
  const replaceTasks = (next: PendingTask[]) => {
    requireWriter()
    writeStorage(scoped("pending-tasks"), next, true)
    tasksRef.current = next
    if (current()) setPendingTasks(next)
  }
  const replaceRemoteResolutions = (next: PendingRemoteResolution[]) => {
    requireStorageWriter()
    writeStorage(scoped("pending-remote-resolutions"), next, true)
    resolutionsRef.current = next
    if (current()) setPendingRemoteResolutions(next)
  }
  const replaceRemoteRebinds = (next: PendingRebind[]) => {
    requireStorageWriter()
    writeStorage(scoped("pending-remote-rebinds"), next, true)
    rebindsRef.current = next
    if (current()) setPendingRemoteRebinds(next)
  }
  const verifyTask = (task: TaskRecord, id: string) => verifyTaskCreation(connection, id, task, tasksRef.current.find(item => item.conversation_id === id && item.task_id === task.task_id)?.attempts)
  const previousExecutionKey = (proof: unknown) => {
    const binding = record(record(proof).binding)
    if (binding.previous_seal === null) return undefined
    const target = text(record(record(binding.previous_seal).seal).target_node)
    const known = [connection.audiencePublicKey, ...remotesRef.current.map(item => item.share.selection.target_public_key),
      ...rebindsRef.current.flatMap(item => [item.plan.oldAuthorization.share.selection.target_public_key, item.plan.nextAuthorization.share.selection.target_public_key])]
    const keys = [...new Set(known.filter(key => nodeFingerprint(key) === target))]
    if (keys.length !== 1) throw new Error("尚未独立保存原执行端公钥，请先核对原 C 信任。")
    return keys[0]
  }
  const currentRemoteTrust = (remote: TrustedRemoteExecution) => ({ ...remote, trust: connection.currentRemoteExecutionTrust(remote.trust) })
  const saveRemoteTrust = async (remote: TrustedRemoteExecution) => {
    requireCurrent()
    const trust = await connection.persistRemoteExecutionTrust(remote.trust)
    requireCurrent()
    return currentRemoteTrust({ ...remote, trust })
  }
  const verifyRemoteScope = async (remote: TrustedRemoteExecution, rawContext: Record<string, unknown>, action: string, use?: RemoteExecutionPolicyUse) => {
    const initial = await connection.remote(remote, rawContext, action, use)
    let trusted: TrustedRemoteExecution
    try { trusted = await saveRemoteTrust({ ...remote, trust: initial.trust }) }
    finally { initial.release() }
    return { authorization: trusted, verified: await connection.remote(trusted, rawContext, action, use) }
  }
  const remoteForSession = (targetNode: string, sessionId: string, taskRef: string, purpose: RemoteExecutionPurpose = 'execution') => {
    if (targetNode === nodeFingerprint(connection.audiencePublicKey)) return undefined
    const matches = remotesRef.current.filter(remote => {
      const proposed = remoteExecutionProposal(remote)
      return proposed.purpose === purpose && nodeFingerprint(remote.share.selection.target_public_key) === targetNode && proposed.sessionId === sessionId && proposed.taskRef === taskRef
    })
    if (matches.length !== 1) throw new Error(purpose === 'rebind' ? "请在原执行会话中选择独立的 Rebind 授权。" : purpose === 'resolution' ? "请导入此任务和执行会话独立的结果处理分享，并确认 C 账户根。" : "请导入此任务和执行会话的远端执行分享，并独立确认 C 账户根。")
    return currentRemoteTrust(matches[0])
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
    window.history.replaceState(null, "", conversationHref(window.location.href, id, connection.scope, connection.project.id))
  }
  const readConversation = async (id: string) => {
    const sequence = ++readSequence.current
    const basis = readFrontiers()[id] ?? []
    const next = parseConversation(await connection.rpc("conversation", { conversation_id: id, basis_heads: basis }), id)
    requireCurrent()
    if (activeId.current !== id || sequence !== readSequence.current) throw new Error("会话读取已更新，请重新核对。")
    for (const task of next.tasks) verifyTask(task, id)
    for (const session of next.sessions) {
      const task = next.tasks.find(task => task.task_id === session.task_id)
      if (!task || session.authority_public_key !== connection.audiencePublicKey || !same(session.task_creation, task.task_creation)) throw new Error("执行会话缺少对应工单的可信创建证明。")
    }
    if (next.operations.some(operation => operation.request.space_id !== spaceId || operation.request.conversation_id !== id)) throw new Error("操作记录属于其他空间或会话。")
    if (!isIncomplete(next.missing)) {
      const latest = readFrontiers()
      const concurrent = (latest[id] ?? []).filter((head) => !basis.includes(head))
      writeStorage(scoped("frontiers"), { ...latest, [id]: [...new Set([...next.heads, ...concurrent])] }, true)
    }
    dataRef.current = next
    setData(next)
    return next
  }
  const refreshNotificationButton = () => {
    if (!canReadNotifications) { setNotificationButton({ label: "当前项目仅供读取历史", disabled: true }); return }
    if (!("Notification" in window)) { setNotificationButton({ label: "浏览器不支持提醒", disabled: true }); return }
    const enabled = readStorage(scoped("notify"), false)
    setNotificationButton(Notification.permission === "granted"
      ? { label: enabled ? "浏览器提醒已开启" : "启用此网络提醒", disabled: enabled }
      : Notification.permission === "denied" ? { label: "请在浏览器设置允许提醒", disabled: true }
      : { label: "开启浏览器提醒", disabled: false })
  }
  const notifyBrowser = (items: NotificationRecord[]) => {
    if (!canReadNotifications) return
    if (!("Notification" in window) || Notification.permission !== "granted" || !readStorage(scoped("notify"), false)) return
    const seen = new Set(readStorage<string[]>(scoped("seen"), []))
    for (const item of items) {
      if (seen.has(item.notification_id)) continue
      seen.add(item.notification_id)
      try {
        const popup = new Notification("Vibeface · 任务进展", { body: item.text, tag: scoped(item.notification_id) })
        popups.current.add(popup)
        popup.onclose = () => popups.current.delete(popup)
        popup.onclick = () => {
          if (!current()) { popup.close(); return }
          window.focus(); selectConversation(item.conversation_id); setView("chat")
          void refreshRef.current().catch(showError)
          popup.close()
        }
      } catch { /* The persisted inbox remains available when browser notifications fail. */ }
    }
    writeStorage(scoped("seen"), [...seen].slice(-1000))
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
        connection.rpc("nodes"), connection.rpc("conversations"), canReadNotifications ? connection.rpc("notifications") : Promise.resolve({ notifications: [] }),
      ])
      requireCurrent()
      const nextNodes = parseNodes(rawNodes), nextConversations = parseConversations(rawConversations), nextNotifications = parseNotifications(rawNotifications)
      setNodes(nextNodes); setConversations(nextConversations); setNotifications(nextNotifications)
      notificationsRef.current = nextNotifications
      if (!activeId.current && nextConversations[0]) selectConversation(nextConversations[0].conversation_id)
      if (activeId.current) await readConversation(activeId.current)
      requireCurrent()
      setSchedulerNodeId((previous) => {
        const choices = nextNodes.filter((node) => !node.revoked && node.roles.includes("scheduler") && node.capabilities?.manager)
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
    const storedPending = parsePending(readStorage<unknown>(scoped("pending"), [], true), spaceId)
    const storedMessages = parsePendingMessages(readStorage<unknown>(scoped("pending-messages"), [], true))
    const storedConversation = parsePendingConversation(readStorage<unknown>(scoped("pending-conversation"), null, true))
    const storedTasks = parsePendingTasks(readStorage<unknown>(scoped("pending-tasks"), [], true))
    const storedBindings = parsePendingBindings(readStorage<unknown>(scoped("pending-bindings"), [], true), spaceId)
    const storedResolutions = parsePendingRemoteResolutions(readStorage<unknown>(scoped("pending-remote-resolutions"), [], true), spaceId)
    const storedRebinds = parsePendingRebinds(readStorage<unknown>(scoped("pending-remote-rebinds"), [], true), spaceId, connection.project.id)
    pendingRef.current = storedPending; setPending(storedPending)
    messagesRef.current = storedMessages; setPendingMessages(storedMessages)
    creationsRef.current = storedConversation; setPendingConversation(storedConversation)
    tasksRef.current = storedTasks; setPendingTasks(storedTasks)
    bindingsRef.current = storedBindings; setPendingBindings(storedBindings)
    resolutionsRef.current = storedResolutions; setPendingRemoteResolutions(storedResolutions)
    rebindsRef.current = storedRebinds; setPendingRemoteRebinds(storedRebinds)
    const storedRemotes = readStorage<unknown>(scoped("remote-executions"), [], true)
    if (!Array.isArray(storedRemotes)) throw new Error("已保存的远端授权分享格式无效。")
    remotesRef.current = storedRemotes.map(value => { const remote = parseTrustedRemoteExecution(value); remoteExecutionProposal(remote); return remote })
    setRemoteExecutions(remotesRef.current)
  }

  useEffect(() => {
    alive.current = true
    observationsRef.current.clear(); setRemoteObservations({})
    try {
      const params = new URL(window.location.href).searchParams
      const matchingScope = params.get("account") === connection.scope.account_id && params.get("space") === connection.scope.space_id && params.get("device") === connection.scope.device_id && params.get("project") === connection.project.id
      if (!matchingScope) {
        for (const key of ["conversation", "basis", "account", "space", "device", "project"]) params.delete(key)
        const clean = new URL(window.location.href); clean.search = params.toString(); window.history.replaceState(null, "", clean)
      }
      activeId.current = params.get("conversation") ?? ""
      setConversationId(activeId.current)
      loadPending()
      const frontiers = readFrontiers(), heads = frontiers[activeId.current] ?? []
      const basis = mergeBasis(heads, params.get("basis"))
      if (activeId.current && basis !== heads) writeStorage(scoped("frontiers"), { ...frontiers, [activeId.current]: basis }, true)
      setBooted(true)
    } catch (caught) { showError(caught) }
    return () => {
      alive.current = false; readSequence.current += 1
      for (const popup of popups.current) popup.close()
      popups.current.clear()
      remoteSessions.current.clear()
      observationsRef.current.clear()
    }
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
    const unsubscribe = connection.subscribe(scheduleRefresh)
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

  const withStorageMutation = async <T,>(key: string, work: () => Promise<T>) => {
    requireStorageWriter()
    if (!booted) throw new Error("本地待核对记录尚未读取，已暂停写入。")
    if (inFlight.current.has(key)) throw new Error("原请求尚未结束，请等待并核对结果。")
    inFlight.current.add(key)
    try { return await work() } finally { inFlight.current.delete(key) }
  }
  const withMutation = async <T,>(key: string, work: () => Promise<T>) => {
    requireWriter()
    return withStorageMutation(key, work)
  }
  const executionScope = async (task: TaskRecord, request: Pick<OperationRequest, "conversation_id" | "task_id" | "target_node" | "session_id">, open: Extract<OperateAction, { type: "open" }>, proof: unknown, originalRemote?: TrustedRemoteExecution, action = "operation_status_read", receipt?: Record<string, unknown>) => {
    const { taskRef } = verifyTask(task, request.conversation_id)
    const remote = originalRemote ? currentRemoteTrust(originalRemote) : remoteForSession(request.target_node, request.session_id, taskRef)
    const executionPublicKey = remote?.share.selection.target_public_key ?? connection.audiencePublicKey
    const binding = verifyExecutionBinding(connection, task, request, open, proof, executionPublicKey, previousExecutionKey(proof))
    const generation = integer(record(binding.binding).generation)
    const facts: RpcFacts = { taskRef, execution: { sessionId: request.session_id, executorKind: open.executor, canonicalCwd: open.cwd, bindingGeneration: generation } }
    let snapshot: RemoteExecutionSession | undefined
    if (remote) {
      facts.remote = { authorization: remote, targetDevice: remoteExecutionProposal(remote).targetDevice, ...(receipt ? { sessionReceipt: receipt } : {}) }
      const rawContext = prepareContext({ conversation_id: request.conversation_id, task_id: task.task_id, session_id: request.session_id }, connection.audience.device, connection.project.id, facts)
      const { authorization, verified } = await verifyRemoteScope(remote, rawContext, action)
      try {
        requireContext(request.conversation_id)
        if (executionPublicKey !== verified.selection.target_public_key) throw new Error("执行授权改变了已核验的绑定目标。")
        facts.remote.authorization = authorization
        if (receipt) {
          snapshot = verified.session(receipt, record(proof))
          if (snapshot.targetNode !== request.target_node || snapshot.sessionId !== request.session_id || snapshot.taskRef !== taskRef || snapshot.executor !== open.executor || snapshot.cwd !== open.cwd || snapshot.bindingGeneration !== generation) throw new Error("远端执行回执与原工单绑定或规范目录不同。")
        }
      } finally { verified.release() }
    }
    return {
      params: {
        conversation_id: request.conversation_id, task_id: task.task_id, task_ref: taskRef,
        target_node: request.target_node, session_id: request.session_id, binding,
        executor: open.executor, cwd: open.cwd, binding_generation: generation,
        ...(remote ? remoteExecutionParams(remote, receipt) : {}),
      },
      facts, snapshot,
    }
  }
  const operationScope = async (request: OperationRequest, remote?: TrustedRemoteExecution, receipt?: Record<string, unknown>) => {
    const context = requireContext(request.conversation_id)
    if (request.space_id !== spaceId) throw new Error("原操作属于其他空间，已保留待核对记录。")
    const task = context.tasks.find(item => item.task_id === request.task_id)
    if (!task) throw new Error("原操作缺少已验证的工单，请先读取原会话。")
    if (request.action.type === "open") return executionScope(task, request, request.action, request.binding, remote, "execution_open", receipt)
    const session = context.sessions.find(item => item.target_node === request.target_node && item.session_id === request.session_id && item.task_id === task.task_id)
    if (!session) throw new Error("原操作缺少已验证的执行会话，请先读取原会话。")
    const action = request.action.type === "read" ? "operation_status_read" : request.action.type === "respond_request" ? "execution_respond_approval" : `execution_${request.action.type}`
    const scope = await executionScope(task, request, { type: "open", executor: session.executor, cwd: session.cwd }, request.binding, remote, action, receipt ?? remoteSessions.current.get(`${request.target_node}:${request.session_id}`)?.receipt)
    if (!same(scope.params.binding, session.binding)) throw new Error("原操作绑定与当前执行会话不同，已保留待核对记录。")
    return scope
  }
  const projectMetadataScope = (request: OperationRequest) => {
    const context = requireContext(request.conversation_id)
    const task = context.tasks.find(task => task.task_id === request.task_id)
    const operation = context.operations.find(operation => operation.request.operation_id === request.operation_id)
    if (!task || !operation?.execution_source || !same(record(operation.raw).request, request)) throw new Error("B 项目尚未提供原操作的签名执行历史。保留现有记录，尚未核实远端结果。")
    const { taskRef } = verifyTask(task, request.conversation_id)
    const metadata = { source: operation.execution_source, binding: request.binding }
    const verified = connection.projectExecutionMetadata(metadata.source, metadata.binding)
    const facts: RpcFacts = { taskRef, projectExecutionMetadata: metadata }
    const params = { conversation_id: request.conversation_id, task_id: task.task_id, task_ref: taskRef, target_node: request.target_node, session_id: request.session_id, binding: request.binding, execution_source: metadata.source, operation_id: request.operation_id }
    prepareContext(params, connection.audience.device, connection.project.id, facts, verified)
    return { params, facts }
  }
  const verifyRemoteObservation = async (request: OperationRequest, actual: Record<string, unknown>, remote?: TrustedRemoteExecution, original?: Parameters<ReadyConnection['rpcWithProof']>[4]) => {
    const context = requireContext(request.conversation_id), task = context.tasks.find(task => task.task_id === request.task_id)
    const open = parseAction(record(record(request.binding).binding).open_spec)
    if (!task || open.type !== "open") throw new Error("原执行缺少已核验的任务或打开绑定。")
    if (original) {
      const candidate = remote ?? remoteForSession(request.target_node, request.session_id, verifyTask(task, request.conversation_id).taskRef)
      if (!candidate) throw new Error("原结果缺少独立保存的执行端根信任。")
      const observation = await connection.remoteExecutionResultHistory(currentRemoteTrust(candidate), actual, original, request.binding)
      requireContext(request.conversation_id)
      if (observation.operationId !== request.operation_id) throw new Error("C 的完整历史回执属于其他原操作。")
      remoteSessions.current.delete(`${request.target_node}:${request.session_id}`)
      observationsRef.current.set(request.operation_id, observation)
      setRemoteObservations(previous => ({ ...previous, [request.operation_id]: observation }))
      const result = observation.result
      setActual(previous => ({ ...previous, [`${request.target_node}:${request.session_id}`]: result !== null && typeof result === 'object' && !Array.isArray(result) && 'actual' in result ? result.actual : result }))
      return observation
    }
    const scoped = await executionScope(task, request, open, request.binding, remote, "operation_status_read")
    if (!scoped.facts.remote) throw new Error("原操作没有独立核验的远端执行授权。")
    const rawContext = prepareContext({ conversation_id: request.conversation_id, task_id: request.task_id, session_id: request.session_id }, connection.audience.device, connection.project.id, scoped.facts)
    const verified = await connection.remote(scoped.facts.remote.authorization, rawContext, "operation_status_read")
    try {
      const observation = verified.observation(actual, request.binding)
      if (observation.operationId !== request.operation_id) throw new Error("C 的完整回执属于其他原操作，已保留待核对记录。")
      const session = record(observation.receipt.payload).session
      if (session !== null && session !== undefined) {
        const expiresAt = record(session).expires_at_ms
        if (typeof expiresAt === "number" && expiresAt > Date.now()) remoteSessions.current.set(`${request.target_node}:${request.session_id}`, verified.session(observation.receipt, request.binding))
      }
      observationsRef.current.set(request.operation_id, observation)
      setRemoteObservations(previous => ({ ...previous, [request.operation_id]: observation }))
      const result = observation.result
      const actualResult = result !== null && typeof result === "object" && !Array.isArray(result) && "actual" in result ? result.actual : result
      setActual(previous => ({ ...previous, [`${request.target_node}:${request.session_id}`]: actualResult }))
      return observation
    } finally { verified.release() }
  }
  const openRemoteResolver = async (request: OperationRequest, remote: TrustedRemoteExecution) => {
    if (remoteExecutionProposal(remote).purpose !== 'resolution') throw new Error("处理未知结果需要独立的结果处理授权。")
    const context = requireContext(request.conversation_id), task = context.tasks.find(task => task.task_id === request.task_id)
    const open = parseAction(record(record(request.binding).binding).open_spec)
    if (!task || open.type !== "open") throw new Error("原裁定缺少已验证的任务和执行绑定。")
    const scoped = await executionScope(task, request, open, request.binding, remote, "operation_resolve")
    if (!scoped.facts.remote) throw new Error("原裁定没有独立保存的 C 授权。")
    const rawContext = prepareContext({ conversation_id: request.conversation_id, task_id: request.task_id, session_id: request.session_id }, connection.audience.device, connection.project.id, scoped.facts)
    const handle = await connection.remote(scoped.facts.remote.authorization, rawContext, "operation_resolve")
    return { handle, facts: scoped.facts, rawContext }
  }
  const resolutionPayload = (response: Record<string, unknown>) => {
    const candidates = [response.actual, response.operation === null || response.operation === undefined ? null : record(response.operation).result]
    for (const candidate of candidates) {
      if (candidate !== null && typeof candidate === "object" && !Array.isArray(candidate) && ("resolution" in candidate || "reconciliation" in candidate)) return record(candidate)
    }
    return null
  }
  const clearRemoteResolution = (pending: PendingRemoteResolution) => {
    removePending(pending.request.operation_id)
    replaceBindings(bindingsRef.current.filter(item => item.request.operation_id !== pending.request.operation_id))
    replaceRemoteResolutions(resolutionsRef.current.filter(item => item.request.operation_id !== pending.request.operation_id))
    remoteSessions.current.delete(`${pending.request.target_node}:${pending.request.session_id}`)
    observationsRef.current.delete(pending.request.operation_id)
    setRemoteObservations(previous => { const next = { ...previous }; delete next[pending.request.operation_id]; return next })
    setActual(previous => { const next = { ...previous }; delete next[`${pending.request.target_node}:${pending.request.session_id}`]; return next })
  }
  const sendRemoteResolution = async (pending: PendingRemoteResolution) => {
    if (pending.body.method !== 'resolve_unknown' || pending.body.space_id !== spaceId || pending.body.operation_id !== pending.request.operation_id || pending.body.authorization_id === pending.request.operation_id || pending.attempt && !same(pending.attempt.body, pending.body)) throw new Error("原裁定的完整正文、签名或独立授权编号不同，已保留待核对记录。")
    const authorizationId = text(pending.body.authorization_id), metadata = projectMetadataScope(pending.request)
    const status = record(await connection.rpc("operation", metadata.params, metadata.facts))
    requireCurrent()
    const historicalOperation = parseOperation(status.operation)
    if (!same(record(historicalOperation.raw).request, pending.request)) throw new Error("B 历史返回了其他原操作，已保留待核对记录。")
    const historical = resolutionPayload(status)
    if (historical && !pending.attempt) throw new Error("B 已有结果处理记录，但此浏览器没有对应原签名。请核对已有结果，已暂停新请求。")
    if (historical && pending.attempt) {
      const attempt = pending.attempt
      const finished = await connection.remoteResolutionHistory(currentRemoteTrust(pending.remote), historical, attempt)
      requireContext(pending.request.conversation_id)
      if (historicalOperation.state !== finished.state) throw new Error("B 历史与原裁定的完整签名回执不同，已保留待核对记录。")
      clearRemoteResolution(pending)
      setNoticeText("原裁定已完成，完整双方签名回执已核验。没有重发执行操作。")
      if (activeId.current === pending.request.conversation_id) await refreshRef.current()
      return
    }
    if (historicalOperation.state !== 'unknown') throw new Error("B 的原操作已结束，但尚未取得与原签名对应的双方裁定回执。请继续核对，已暂停发送。")
    requireWriter()
    const resolver = await openRemoteResolver(pending.request, pending.remote)
    try {
      const intent = resolver.handle.resolution(pending.observation, authorizationId, text(pending.body.outcome), text(pending.body.evidence))
      if (!same(intent.body, pending.body) || !same(intent.context, pending.context) || !same(resolver.rawContext, pending.context)) throw new Error("原裁定的受签范围或证据已变化，已保留原请求。")
      const { method, space_id: bodySpace, authorization_id: bodyAuthorizationId, ...params } = pending.body
      if (method !== "resolve_unknown" || bodySpace !== spaceId || bodyAuthorizationId !== authorizationId) throw new Error("原裁定的方法、空间或授权编号无效。")
      if (!resolver.facts.remote) throw new Error("原裁定的独立远端授权缺失。")
      resolver.facts.authorizationId = authorizationId
      const reply = await connection.rpcWithProof("resolve_unknown", params, attempt => {
        const original = resolutionsRef.current.find(item => item.request.operation_id === pending.request.operation_id)
        if (!original || !same(original.body, pending.body) || !same(attempt.body, pending.body) || original.attempt && !same(original.attempt, attempt)) throw new Error("原裁定的完整签名或待核对记录不同，已暂停发送。")
        replaceRemoteResolutions(resolutionsRef.current.map(item => item === original ? { ...item, attempt } : item))
      }, resolver.facts, pending.attempt ?? undefined)
      requireCurrent()
      const response = record(reply.result), actual = resolutionPayload(response)
      if (!actual) throw new Error("B 已接收原裁定，但未返回可独立验证的双方回执。已保存原签名，继续核对原裁定。")
      const finished = await connection.remoteResolutionHistory(currentRemoteTrust(pending.remote), actual, reply)
      requireContext(pending.request.conversation_id)
      const operation = parseOperation(response.operation)
      if (!same(record(operation.raw).request, pending.request) || operation.state !== finished.state) throw new Error("B 的裁定状态与双方完整签名结果不同，已保留原请求。")
      clearRemoteResolution(pending)
      setNoticeText(`双方裁定回执已独立核验（${stateNames[finished.state] ?? finished.state}）。原操作已解除占用，没有重新执行。`)
      if (activeId.current === pending.request.conversation_id) await refreshRef.current()
    } finally { resolver.handle.release() }
  }
  const sendOperation = async (request: OperationRequest, pendingBindingId?: string, remote?: TrustedRemoteExecution) => {
    const scope = await operationScope(request, remote)
    if (scope.facts.remote && request.action.type !== "open" && (!scope.snapshot || scope.snapshot.revision !== request.expected_revision)) throw new Error("远端会话需先取得并独立验证 C 的当前执行回执和版本，已暂停新操作。")
    const pendingOperation: PendingOperation = { space_id: request.space_id, operation_id: request.operation_id, target_node: request.target_node, session_id: request.session_id, conversation_id: request.conversation_id, request, attempt: null, ...(scope.facts.remote ? { remote: scope.facts.remote.authorization, ...(scope.facts.remote.sessionReceipt ? { execution_session_receipt: scope.facts.remote.sessionReceipt } : {}) } : {}) }
    replacePending([...pendingRef.current, pendingOperation])
    const reply = await connection.rpcWithProof("operate", { request, task_ref: scope.params.task_ref, ...(scope.facts.remote ? remoteExecutionParams(scope.facts.remote.authorization, scope.facts.remote.sessionReceipt) : {}) }, attempt => {
      const original = pendingRef.current.find(item => item.operation_id === request.operation_id)
      if (!original || !same(original.request, request)) throw new Error("原操作待核对记录已变化，已暂停发送。")
      const normalized = parseOperationRequest(attempt.body.request, spaceId)
      replacePending(pendingRef.current.map(item => item.operation_id === request.operation_id ? { ...item, request: normalized, attempt } : item))
    }, scope.facts)
    const result = parseOperation(record(reply.result).operation)
    requireCurrent()
    if (!same(record(result.raw).request, reply.body.request)) throw new Error("响应操作与原请求不一致，请核对原操作。")
    if (scope.facts.remote) {
      const actual = record(reply.result).actual
      if (actual !== null && actual !== undefined) {
        const observation = await verifyRemoteObservation(request, record(actual), scope.facts.remote.authorization, reply)
        if (result.state !== observation.state && result.state !== "unknown") throw new Error("B 的持久状态与 C 的签名结果不同，已保留原请求。")
        if (result.state === observation.state && result.state !== "unknown") {
          if (pendingBindingId) replaceBindings(bindingsRef.current.filter(item => item.request.operation_id !== pendingBindingId))
          if (["succeeded", "failed", "rejected"].includes(observation.state)) removePending(request.operation_id)
        }
        setNoticeText(result.state === "unknown" ? "已核验 C 的完整结果。B 的原操作仍未知，需要明确确认核查结果后处理。" : `C 的完整签名结果已核验（${stateNames[observation.state] ?? observation.state}）。`)
      } else {
        setNoticeText("B 项目已持久记录原操作。C 当前未能核实，原请求继续保留。")
      }
    } else {
      if (pendingBindingId && result.state !== "unknown") replaceBindings(bindingsRef.current.filter(item => item.request.operation_id !== pendingBindingId))
      if (["succeeded", "failed", "rejected"].includes(result.state)) removePending(request.operation_id)
      setNoticeText(`操作已提交（${stateNames[result.state] ?? result.state}）。`)
    }
    if (activeId.current === request.conversation_id) await refreshRef.current()
  }
  const completeOpen = async (pendingBinding: PendingBinding) => {
    const { request, task } = pendingBinding, context = requireContext(request.conversation_id)
    if (pendingRef.current.some(item => item.operation_id === request.operation_id || item.target_node === request.target_node && item.conversation_id === request.conversation_id)) throw new Error("执行设备有尚未确认的操作，请先核对原操作。")
    const currentTask = context.tasks.find(item => item.task_id === task.task_id)
    if (!currentTask || !same(currentTask.task_creation, task.task_creation)) throw new Error("原工单创建证明已变化，请保留待核对绑定并核查。")
    if (["accepted", "cancelled"].includes(currentTask.status)) throw new Error("工单已结束，已保留原绑定记录，请先核对工单状态。")
    const creation = verifyTask(task, request.conversation_id)
    const remote = pendingBinding.remote ? currentRemoteTrust(pendingBinding.remote) : undefined
    if (!remote && request.target_node !== nodeFingerprint(connection.audiencePublicKey)) throw new Error("原绑定缺少独立确认的远端授权，已保留待核对记录。")
    let executionPublicKey = remote?.share.selection.target_public_key ?? connection.audiencePublicKey
    const bindingGeneration = currentTask.binding === null ? 1 : integer(record(verifyExecutionBinding(connection, currentTask, request, request.action,
      currentTask.binding, executionPublicKey, previousExecutionKey(currentTask.binding)).binding).generation)
    const facts: RpcFacts = { taskRef: creation.taskRef, execution: { sessionId: request.session_id, executorKind: request.action.executor, canonicalCwd: request.action.cwd, bindingGeneration } }
    if (remote) {
      facts.remote = { authorization: remote, targetDevice: remoteExecutionProposal(remote).targetDevice }
      const rawContext = prepareContext({ conversation_id: request.conversation_id, task_id: task.task_id, session_id: request.session_id }, connection.audience.device, connection.project.id, facts)
      const { authorization, verified } = await verifyRemoteScope(remote, rawContext, "execution_open")
      try {
        requireContext(request.conversation_id)
        executionPublicKey = verified.selection.target_public_key
        facts.remote.authorization = authorization
      } finally { verified.release() }
    }
    let proof: unknown = currentTask.binding
    if (proof === null) {
      const reply = await connection.rpcWithProof("bind_execution", {
        conversation_id: request.conversation_id, task_id: task.task_id, task_ref: creation.taskRef,
        target_node: request.target_node, session_id: request.session_id, executor: request.action.executor, cwd: request.action.cwd,
        binding_generation: bindingGeneration,
        ...(remote ? remoteExecutionParams(remote) : {}),
      }, attempt => {
        if (pendingBinding.attempt && !same(pendingBinding.attempt, attempt)) throw new Error("绑定重试的签名与原请求不同，请先核对。")
        const next = { ...pendingBinding, attempt }
        replaceBindings(bindingsRef.current.map(item => item.request.operation_id === request.operation_id ? next : item))
      }, facts, pendingBinding.attempt ?? undefined)
      requireContext(request.conversation_id)
      proof = record(reply.result).binding
    }
    const binding = verifyExecutionBinding(connection, task, request, request.action, proof, executionPublicKey, previousExecutionKey(proof))
    await sendOperation({ ...request, binding }, request.operation_id, remote)
  }
  const run = async (action: () => Promise<void>) => {
    if (current()) setError("")
    try { await action() } catch (caught) { showError(caught) }
  }
  const incomplete = !data || isIncomplete(data.missing)
  const messagePending = pendingMessages.find((item) => item.conversation_id === conversationId) ?? null
  const selectedScheduler = nodes.find((node) => node.node_id === (messagePending?.scheduler_node ?? schedulerNodeId))
  const chatSlot = {
    conversationId: conversationId || null, schedulerNodeId: selectedScheduler?.node_id ?? null,
    missing: incomplete, schedulerOnline: Boolean(selectedScheduler?.online && !selectedScheduler.revoked), managerReady: Boolean(selectedScheduler?.capabilities?.manager),
  }

  return {
    info: connection.info, projectNodeId: nodeFingerprint(connection.audiencePublicKey), projectScopeId: scoped("workspace"), connectionOnline: current(), connectionText: "已建立端到端加密连接",
    error, noticeText, view, setView, nodes, conversations, conversationId, data, notifications, pending, pendingBindings, actual, remoteExecutions, remoteObservations, pendingRemoteResolutions, pendingRemoteRebinds,
    canAutoFollowup, pendingConversation, pendingTask: pendingTasks.find((item) => item.conversation_id === conversationId) ?? null,
    refreshing, schedulerNodeId, setSchedulerNodeId, incomplete, chatSlot, messagePending, messageBusy,
    writable: canWrite && writeState === "writable",
    localWritable: writeState === "writable",
    writeNotice: !canWrite ? "当前项目授权仅供读取历史。写入、任务派发和浏览器通知不可用。"
      : writeState === "readonly" ? "另一个页面正在操作此浏览器设备。此页只读，关闭其他页面后重新连接。"
      : writeState === "unsupported" ? "浏览器未能提供跨页面写锁，此页只读。请使用支持 Web Locks 的安全浏览器入口。"
      : writeState === "checking" ? "正在核对浏览器设备的写入权限。" : "",
    syncText: !data ? "选择或创建会话后，可发消息和派发任务。" : incomplete
      ? `上下文尚未完整，已暂停派发。缺少事件 ${missingCount(data.missing.events)}，附件 ${missingCount(data.missing.blobs)}，冲突 ${missingCount(data.missing.conflicts)}。`
      : `此入口已取得已知上下文，共 ${data.heads.length} 个历史位置。其他离线设备可能仍有未同步记录。`,
    selectConversation, refresh, run, announce: setNoticeText, notificationButton,
    exportRemoteExecutionSourceTrust: () => { requireCurrent(); return connection.exportExecutionSourceTrust() },
    remoteExecutionTarget: (remote: TrustedRemoteExecution) => {
      const context = requireContext(), proposed = remoteExecutionProposal(remote)
      if (proposed.purpose !== 'execution') throw new Error("结果处理授权不能打开或操作执行会话。")
      const task = context.tasks.find(task => verifyTask(task, activeId.current).taskRef === proposed.taskRef)
      if (!task || proposed.conversationId !== activeId.current) throw new Error("此远端授权属于其他任务或会话。")
      return { target_node: nodeFingerprint(remote.share.selection.target_public_key), session_id: proposed.sessionId, task_id: task.task_id, executor: proposed.executorKind }
    },
    remoteExecutionRequest: async (taskId: string, executorKind: string, expiresAt: number, purpose: RemoteExecutionPurpose = 'execution', forRebind = false) => {
      const context = requireContext(), task = context.tasks.find(task => task.task_id === taskId)
      if (!task || task.binding === null && ["accepted", "cancelled"].includes(task.status)) throw new Error("请选择待执行任务，或已有原执行绑定的任务以续申请。")
      if (!["codex", "claude_code", "cursor_agent"].includes(executorKind)) throw new Error("请选择支持的执行器。")
      if (!Number.isSafeInteger(expiresAt) || expiresAt <= Date.now()) throw new Error("明确选择晚于当前时刻的申请有效期。")
      if (!['execution', 'resolution', 'rebind'].includes(purpose) || forRebind && purpose !== 'execution') throw new Error("请明确选择本次申请的独立用途；新执行目标只接受执行用途。")
      const { taskRef } = verifyTask(task, activeId.current)
      const original = task.binding === null ? null : verifyBoundExecutionSelection(connection, activeId.current, task, previousExecutionKey(task.binding))
      if (forRebind && !original) throw new Error("请先选择已有原执行绑定的任务，再为重绑申请新目标。")
      if (forRebind && ["accepted", "cancelled"].includes(task.status)) throw new Error("工单已结束，不能为它申请新的重绑执行目标。")
      if (original && !forRebind && original.executorKind !== executorKind) throw new Error("续申请必须使用原任务绑定的执行器。")
      if (purpose !== 'execution' && !original) throw new Error("结果处理与原 C 封存申请必须使用已有执行绑定。")
      const oldBinding = original ? record(record(task.binding).binding) : null
      const prior = oldBinding && original && purpose !== 'execution' ? remoteForSession(text(oldBinding.target_node), original.sessionId, taskRef) : undefined
      if (purpose !== 'execution' && !prior) throw new Error("请先独立核验原远端执行授权。")
      const receipt = purpose === 'rebind' && oldBinding && original ? remoteSessions.current.get(`${text(oldBinding.target_node)}:${original.sessionId}`)?.receipt : undefined
      if (purpose === 'rebind' && !receipt) throw new Error("请先核验原 C 的当前执行回执，再申请单独的封存授权。")
      const result = await connection.createRemoteExecutionRequest({ conversationId: activeId.current, taskId: task.task_id, taskRef, taskCreation: task.task_creation,
        sessionId: forRebind ? newId() : original?.sessionId ?? newId(), executorKind, expiresAt, purpose,
        ...(prior ? { executionResource: prior.share.selection.execution_resource, targetPublicKey: prior.share.selection.target_public_key } : {}),
        ...(purpose === 'rebind' ? { oldBinding: record(task.binding), oldExecutionReceipt: receipt } : {}) })
      requireCurrent()
      record(JSON.parse(result))
      return result
    },
    importRemoteExecution: (shareJson: string, independentTrustJson: string, cwd: string) => withMutation("import-remote-execution", async () => {
      const context = requireContext(), share = parseRemoteExecutionShare(JSON.parse(shareJson))
      const remote = parseTrustedRemoteExecution({ share, trust: JSON.parse(independentTrustJson) })
      const proposed = remoteExecutionProposal(remote)
      if (proposed.purpose === 'rebind') throw new Error("请在原执行会话的重绑表单中导入这份 Rebind 授权。")
      const task = context.tasks.find(task => verifyTask(task, activeId.current).taskRef === proposed.taskRef)
      if (!task || proposed.conversationId !== activeId.current) throw new Error("分享未精确授权当前会话的一张已验证任务。")
      const canonicalCwd = inputText(cwd, "远端规范工作目录", 4096)
      const candidate = remote
      let bindingGeneration = 1, action = proposed.purpose === 'resolution' ? "operation_resolve" : "execution_open"
      if (task.binding !== null) {
        const current = verifyBoundExecutionSelection(connection, activeId.current, task, previousExecutionKey(task.binding))
        const binding = record(task.binding.binding), open = record(binding.open_spec)
        bindingGeneration = integer(binding.generation)
        if (proposed.purpose === 'execution' && (binding.target_node !== nodeFingerprint(candidate.share.selection.target_public_key)
          || current.sessionId !== proposed.sessionId || current.executorKind !== proposed.executorKind || open.cwd !== canonicalCwd)) {
          bindingGeneration += 1
          if (!Number.isSafeInteger(bindingGeneration)) throw new Error("新绑定代次超过此客户端支持范围。")
          action = "execution_rebind"
        }
      }
      const facts: RpcFacts = { taskRef: proposed.taskRef, execution: { sessionId: proposed.sessionId, executorKind: proposed.executorKind, canonicalCwd, bindingGeneration }, remote: { authorization: candidate, targetDevice: proposed.targetDevice } }
      const rawContext = prepareContext({ conversation_id: activeId.current, task_id: task.task_id, session_id: proposed.sessionId }, connection.audience.device, connection.project.id, facts)
      const { authorization: trusted, verified } = await verifyRemoteScope(candidate, rawContext, action, { kind: 'candidate', binding: task.binding })
      try {
        const original = remotesRef.current.find(item => item.share.selection.execution_grant === share.selection.execution_grant)
        if (original && !same(original.share, share)) throw new Error("同一远端 Grant 的分享内容发生变化，请保留原授权并核对。")
        const next = [...remotesRef.current.filter(item => {
          const previous = remoteExecutionProposal(item)
          return previous.purpose !== proposed.purpose || item.share.selection.target_public_key !== share.selection.target_public_key || previous.conversationId !== proposed.conversationId || previous.taskRef !== proposed.taskRef || previous.sessionId !== proposed.sessionId
        }), trusted]
        writeStorage(scoped("remote-executions"), next, true)
        remotesRef.current = next; setRemoteExecutions(next)
        setNoticeText(proposed.purpose === 'resolution' ? "已独立验证结果处理授权，并保存 C 账户根信任。执行授权保持原样。" : "已验证远端有限执行授权，并独立保存 C 账户根信任。尚未执行任务。")
      } finally { verified.release() }
    }),
    prepareRemoteRebind: (session: SessionRecord, oldShareJson: string, nextGrant: string, cwd: string) => withMutation(`rebind:${session.target_node}:${session.session_id}`, async () => {
      const context = requireContext()
      if (!connection.authorization.actions.includes('execution_rebind')) throw new Error("当前 B 项目未授权重绑执行，请先取得明确的重绑权限。")
      const task = context.tasks.find(item => item.task_id === session.task_id)
      if (!task || !same(task.binding, session.binding)) throw new Error("原任务绑定已变化，请先读取当前任务。")
      if (["accepted", "cancelled"].includes(task.status)) throw new Error("工单已结束，不能保存新的执行重绑计划。")
      const { taskRef } = verifyTask(task, activeId.current)
      if (session.state === 'unknown' || pendingRef.current.some(item => item.target_node === session.target_node && item.session_id === session.session_id)) throw new Error("原会话还有未知或待核对操作，请先核对结果，再准备重绑。")
      if (rebindsRef.current.some(item => record(item.plan.binding.binding).task_ref === taskRef)) throw new Error("此任务已有待核对重绑，请保留并继续原计划。")
      const snapshot = remoteSessions.current.get(`${session.target_node}:${session.session_id}`)
      if (!snapshot || snapshot.state === 'unknown') throw new Error("请先核验 C 的当前执行回执并处理未知结果，再准备重绑。")
      const old = remoteForSession(session.target_node, session.session_id, taskRef)
      if (!old) throw new Error("此入口只处理具有独立 C 授权的远端会话重绑。")
      const oldAuthorization = parseTrustedRemoteExecution({ share: parseRemoteExecutionShare(JSON.parse(oldShareJson)), trust: old.trust })
      if (oldAuthorization.share.selection.target_public_key !== old.share.selection.target_public_key
        || !same(oldAuthorization.share.selection.execution_resource, old.share.selection.execution_resource)) throw new Error("Rebind 授权属于其他原执行端或资源。")
      const next = remotesRef.current.find(item => item.share.selection.execution_grant === nextGrant)
      if (!next) throw new Error("请先导入并核验新执行目标的独立执行分享。")
      const planned = prepareRemoteRebind({
        id: newId(), project: { space: spaceId, resource: connection.project.id }, projectPublicKey: connection.audiencePublicKey,
        binding: session.binding, executionReceipt: snapshot.receipt, oldAuthorization, nextAuthorization: currentRemoteTrust(next),
        nextOperationId: newId(), cwd: inputText(cwd, '新执行端规范工作目录', 4096), basisHeads: context.heads,
        expectedRevisionJson: JSON.stringify(snapshot.revision),
      })
      replaceRemoteRebinds([...rebindsRef.current, planned])
      setNoticeText("重绑计划已保存。尚未封存原会话、修改 B 绑定或启动新执行。")
    }),
    discardRemoteRebind: (id: string) => withStorageMutation(`rebind:${id}`, async () => {
      const pending = rebindsRef.current.find(item => item.plan.id === id)
      if (!pending || pending.phase !== 'seal' || pending.attempt !== null) throw new Error("原重绑已有签名或已进入后续阶段，请先核对原操作。")
      replaceRemoteRebinds(rebindsRef.current.filter(item => item !== pending))
      setNoticeText("已取消尚未提交的重绑计划，没有修改执行会话。")
    }),
    resumeRemoteRebind: (id: string) => withStorageMutation(`rebind:${id}`, async () => {
      let pending = rebindsRef.current.find(item => item.plan.id === id)
      if (!pending) throw new Error("原重绑记录已变化，请重新读取。")
      const persist = (next: PendingRebind) => {
        const original = rebindsRef.current.find(item => item.plan.id === id)
        if (!original || !same(original, pending)) throw new Error("原重绑记录已变化，已暂停发送。")
        replaceRemoteRebinds(rebindsRef.current.map(item => item === original ? next : item)); pending = next
      }
      const plan = pending.plan, originalBinding = record(plan.binding.binding), conversationId = text(originalBinding.conversation_id)
      requireContext(conversationId)
      if (pending.phase === 'seal') {
        let seal: unknown = null
        if (pending.attempt) {
          const status = await connection.callExecutionResource(currentRemoteTrust(plan.oldAuthorization), remoteSealStatusBody(pending))
          requireContext(conversationId); seal = record(status.result).seal
          if (seal === undefined) throw new Error("原 C 没有明确确认原封存是否存在，请保留原签名继续核对。")
        }
        if (seal === null) {
          const original = pending
          const reply = await connection.callExecutionResource(currentRemoteTrust(plan.oldAuthorization), original.body, proof => {
            if (!pending || pending.phase !== 'seal') throw new Error("原封存阶段已变化。")
            persist(recordRebindAttempt(pending, proof, signedInvocationHash(JSON.stringify(proof.invocation))))
          }, original.attempt?.proof)
          requireContext(conversationId); seal = record(reply.result).seal
        }
        if (!pending || pending.phase !== 'seal') throw new Error("原封存阶段已变化。")
        const verified = record(JSON.parse(verifyRemoteExecutionSeal(JSON.stringify(plan.binding), JSON.stringify(record(seal)), plan.projectPublicKey,
          plan.oldAuthorization.share.selection.target_public_key, plan.expectedRevisionJson)))
        persist(acceptRebindSeal(pending, verified))
        remoteSessions.current.delete(`${text(originalBinding.target_node)}:${text(originalBinding.session_id)}`)
        setNoticeText("原 C 的精确封存回执已核验并保存。请继续明确提交 B 的新绑定。")
        return
      }
      const fresh = await readConversation(conversationId)
      requireContext(conversationId)
      let task = fresh.tasks.find(item => item.task_id === originalBinding.task_id)
      if (!task || verifyTask(task, conversationId).taskRef !== originalBinding.task_ref) throw new Error("原任务创建证明已变化，请保留重绑记录。")
      if (pending.phase === 'rebind') {
        if (same(task.binding, plan.binding)) {
          requireWriter()
          if (["accepted", "cancelled"].includes(task.status)) throw new Error("工单已结束，已暂停新的 B 重绑写入。")
          const proposed = remoteExecutionProposal(plan.nextAuthorization), generation = integer(originalBinding.generation) + 1
          if (!Number.isSafeInteger(generation)) throw new Error("新绑定代次超过此客户端支持范围。")
          const facts: RpcFacts = { authorizationId: text(pending.body.authorization_id), taskRef: proposed.taskRef,
            execution: { sessionId: proposed.sessionId, executorKind: proposed.executorKind, canonicalCwd: plan.cwd, bindingGeneration: generation },
            remote: { authorization: currentRemoteTrust(plan.nextAuthorization), targetDevice: proposed.targetDevice } }
          const { method: _method, space_id: _space, authorization_id: _authorization, ...params } = pending.body
          const original = pending
          const reply = await connection.rpcWithProof('rebind_execution', params, proof => {
            if (!pending || pending.phase !== 'rebind') throw new Error("原 B 重绑阶段已变化。")
            persist(recordRebindAttempt(pending, proof))
          }, facts, original.attempt ?? undefined)
          requireContext(conversationId)
          const returned = record(reply.result).binding
          const updated = await readConversation(conversationId)
          task = updated.tasks.find(item => item.task_id === originalBinding.task_id)
          if (!task || !same(task.binding, returned)) throw new Error("B 的新绑定尚未与当前完整任务一致，请保留原请求继续核对。")
        }
        if (!pending || pending.phase !== 'rebind' || !pending.attempt) throw new Error("原重绑签名缺失，无法接管新绑定。")
        const next = remoteExecutionProposal(plan.nextAuthorization)
        const binding = verifyExecutionBinding(connection, task, { conversation_id: conversationId, task_id: task.task_id,
          target_node: nodeFingerprint(plan.nextAuthorization.share.selection.target_public_key), session_id: next.sessionId },
          { type: 'open', executor: next.executorKind, cwd: plan.cwd }, task.binding, plan.nextAuthorization.share.selection.target_public_key,
          plan.oldAuthorization.share.selection.target_public_key)
        persist(acceptReboundBinding(pending, binding, requireContext(conversationId).heads))
        setNoticeText("B 的新绑定及旧 C 封存已独立核验。请继续提交新 C 的原打开请求。")
        return
      }
      if (!same(task.binding, pending.binding)) throw new Error("当前任务已改用其他绑定，请保留原打开请求并核对。")
      const request = parseOperationRequest(pending.body.request, spaceId)
      if (request.action.type !== 'open') throw new Error("重绑的后续请求不是原打开操作。")
      const settle = async (value: unknown, proof: { body: Record<string, unknown>; invocation: unknown }) => {
        const response = record(value), operation = parseOperation(response.operation)
        if (!same(record(operation.raw).request, request)) throw new Error("新 C 的操作记录与原打开请求不同。")
        if (response.actual === undefined || response.actual === null) { setNoticeText("已读取 B 的原打开记录。新 C 尚未核实，继续保留原请求。 "); return }
        const observed = await verifyRemoteObservation(request, record(response.actual), plan.nextAuthorization, proof)
        if (operation.state !== observed.state && operation.state !== 'unknown') throw new Error("B 和新 C 的打开结果不同，请继续核对原请求。")
        if (operation.state === observed.state && ['succeeded', 'failed', 'rejected'].includes(observed.state)) {
          replaceRemoteRebinds(rebindsRef.current.filter(item => item.plan.id !== id))
          setNoticeText("已核验原 C 封存、B 新绑定及新 C 打开的完整结果。")
        } else setNoticeText("原打开请求仍在执行或结果未知。继续时先读取原操作，不会生成新的打开请求。")
      }
      if (pending.attempt) {
        const statusScope = await executionScope(task, request, request.action, pending.binding, plan.nextAuthorization, 'operation_status_read')
        const status = record(await connection.rpc('operation', { ...statusScope.params, operation_id: request.operation_id }, statusScope.facts))
        requireContext(conversationId)
        if (status.operation !== null) { await settle(status, pending.attempt); return }
        if (status.actual !== null && status.actual !== undefined) throw new Error("新 C 已返回结果但 B 尚无对应记录，请保留原请求核对。")
      }
      requireWriter()
      if (["accepted", "cancelled"].includes(task.status)) throw new Error("工单已结束，已保留原打开请求，只允许核对已有结果。")
      const scope = await executionScope(task, request, request.action, pending.binding, plan.nextAuthorization, 'execution_open')
      const original = pending
      const { method: _method, space_id: _space, authorization_id: _authorization, ...params } = original.body
      const reply = await connection.rpcWithProof('operate', params, proof => {
        if (!pending || pending.phase !== 'open') throw new Error("原新 C 打开阶段已变化。")
        persist(recordRebindAttempt(pending, proof))
      }, scope.facts, original.attempt ?? undefined)
      requireContext(conversationId); await settle(reply.result, reply)
      await refreshRef.current()
    }),
    createConversation: (proposedId: string, inputTitle: string) => withMutation("create-conversation", async () => {
      const title = inputText(inputTitle, "会话名称", 512)
      const prior = creationsRef.current
      if (prior && prior.title !== title) throw new Error(`还有待确认的会话“${prior.title}”，请保留原名称重试。`)
      if (prior && !prior.attempt) throw new Error("原会话缺少已保存的签名，已保留待核对记录，不能重新签名发送。")
      const pendingConversation = prior ?? { conversation_id: proposedId, title, attempt: null }
      const request = { conversation_id: pendingConversation.conversation_id, title: pendingConversation.title }
      await connection.rpcWithProof("create_conversation", request, attempt => {
        requireWriter()
        const next = { ...pendingConversation, attempt }
        writeStorage(scoped("pending-conversation"), next, true)
        creationsRef.current = next; setPendingConversation(next)
      }, undefined, pendingConversation.attempt ?? undefined)
      requireCurrent()
      writeStorage(scoped("pending-conversation"), null, true)
      creationsRef.current = null; setPendingConversation(null)
      selectConversation(request.conversation_id); setView("chat")
      await refreshRef.current()
    }),
    createTask: (input: { taskId: string; title: string; goal: string; acceptance: string; autoFollowup: boolean }) => withMutation(`create-task:${activeId.current}`, async () => {
      const context = requireContext(), id = context.conversation?.conversation_id
      if (!id) throw new Error("先选择会话。")
      const title = inputText(input.title, "工单标题", 65536)
      const goal = inputText(input.goal, "工单目标", 65536)
      const acceptance = inputText(input.acceptance, "验收条件", 65536)
      if (input.autoFollowup && !canAutoFollowup) throw new Error("当前入口未启用接入角色，不能授权自动继续检查。")
      const prior = tasksRef.current.find((item) => item.conversation_id === id)
      if (prior && (prior.title !== title || prior.goal !== goal || prior.acceptance !== acceptance || prior.auto_followup !== input.autoFollowup)) throw new Error("还有待确认的工单，请保留原内容与继续检查设置重试。")
      const pendingTask = prior ?? { conversation_id: id, task_id: input.taskId, title, goal, acceptance, auto_followup: input.autoFollowup, attempts: [] }
      replaceTasks([...tasksRef.current.filter((item) => item.conversation_id !== id), pendingTask])
      const { attempts: _attempts, ...request } = pendingTask
      const reply = await connection.rpcWithProof("create_task", request, attempt => {
        const original = tasksRef.current.find(item => item.task_id === pendingTask.task_id && item.conversation_id === id)
        if (!original) throw new Error("原工单待核对记录已变化，已暂停发送。")
        const previous = original.attempts.at(-1)
        if (previous && !same(previous, attempt)) throw new Error("工单重试的签名与原请求不同，已暂停发送。")
        const next = { ...original, attempts: previous ? original.attempts : [attempt] }
        replaceTasks(tasksRef.current.map(item => item === original ? next : item))
      }, undefined, pendingTask.attempts.at(-1))
      requireCurrent()
      const result = record(reply.result)
      const task = parseTask({ ...record(result.task), task_creation: result.creation, authority_public_key: result.authority_public_key })
      if (task.task_id !== request.task_id || task.title !== title || task.goal !== goal || task.acceptance !== acceptance) throw new Error("工单回执与原创建内容不同，请保留待核对记录。")
      verifyTask(task, id)
      requireCurrent()
      replaceTasks(tasksRef.current.filter((item) => item.task_id !== pendingTask.task_id))
      if (activeId.current === id) await refreshRef.current()
    }),
    updateTask: (id: string, taskId: string, status: string, detail: string) => withMutation(`update-task:${taskId}`, async () => {
      const context = requireContext(id), task = context.tasks.find(item => item.task_id === taskId)
      if (!task) throw new Error("先选择已创建并验证的工单。")
      const { taskRef } = verifyTask(task, id)
      await connection.rpc("update_task", { conversation_id: id, task_id: taskId, task_ref: taskRef, status, detail }, { taskRef })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    sendMessage: (text: string) => withMutation(`message:${activeId.current}`, async () => {
      const context = requireContext(), id = activeId.current
      const prior = messagesRef.current.find((item) => item.conversation_id === id) ?? null
      const scheduler = nodes.find((item) => item.node_id === (prior?.scheduler_node ?? schedulerNodeId))
      if (!scheduler?.online || scheduler.revoked || !scheduler.capabilities?.manager) throw new Error("原调度设备不可用，请先恢复连接并核对原消息。")
      const prepared = prepareSend({ conversationId: id, text, schedulerNodeId: scheduler.node_id, entryNode: connection.audience.device, pending: prior, heads: context.heads })
      if (!prepared.ok) throw new Error(prepared.error)
      if (prior && !prior.attempt) throw new Error("原消息缺少已保存的签名，已保留待核对记录，不能重新签名发送。")
      setMessageBusy(true)
      try {
        await connection.rpcWithProof("message", prepared.body, attempt => {
          const { rejected: _rejected, ...original } = prepared.pending
          replaceMessages([...messagesRef.current.filter(item => item.conversation_id !== id), { ...original, attempt }])
        }, undefined, prepared.pending.attempt ?? undefined)
        requireCurrent()
        replaceMessages(messagesRef.current.filter((item) => item.message_id !== prepared.pending.message_id))
        setNoticeText("消息已提交，调度 Agent 正在处理。")
        if (activeId.current === id) await refreshRef.current()
      } catch (caught) {
        if (current() && caught instanceof Error && "responded" in caught && caught.responded === true) {
          const original = messagesRef.current.find((item) => item.message_id === prepared.pending.message_id)
          if (original) replaceMessages(messagesRef.current.map((item) => item === original ? { ...item, rejected: { entry_node: connection.audience.device } } : item))
        }
        throw caught
      } finally { if (current()) setMessageBusy(false) }
    }),
    releaseMessagePending: () => withMutation(`message:${activeId.current}`, async () => {
      requireCurrent()
      const id = activeId.current, original = messagesRef.current.find((item) => item.conversation_id === id)
      if (!original) throw new Error("原消息请求尚未结束，请先核对。")
      if (original.entry_node !== connection.audience.device || original.rejected?.entry_node !== connection.audience.device) throw new Error("需要原入口明确返回错误，再在同一连接核对完整会话。超时或断线后不能据此解除，请原样重试并核对。")
      const complete = await readConversation(id)
      requireContext(id)
      if (messagesRef.current.find((item) => item.conversation_id === id) !== original || complete.messages.some((item) => item.message_id === original.message_id)) throw new Error("已找到原消息或待核对记录变化，请保留原记录并核对调度结果。")
      replaceMessages(messagesRef.current.filter((item) => item !== original))
      setNoticeText("已解除本地待核对记录，未重发消息。")
    }),
    operate: (target: { target_node: string; session_id: string; task_id: string; revision?: number }, action: OperateAction, id: string, remote?: TrustedRemoteExecution) => withMutation(`operate:${target.target_node}:${target.session_id}`, async () => {
      const context = requireContext(id)
      if (!remote && action.type === "open" && target.target_node !== nodeFingerprint(connection.audiencePublicKey)) throw new Error("远端执行需使用独立确认的有限授权入口。")
      if (pendingRef.current.some((item) => item.conversation_id === id && item.target_node === target.target_node && (action.type === "open" || item.session_id === target.session_id))) throw new Error("此设备有尚未确认的操作，请先核对原操作。")
      const session = context.sessions.find((item) => item.target_node === target.target_node && item.session_id === target.session_id)
      const task = context.tasks.find(item => item.task_id === target.task_id)
      if (!task) throw new Error("先选择已创建并验证的工单。")
      verifyTask(task, id)
      if (session?.state === "unknown" && action.type !== "read") throw new Error("执行结果未知，请先读取实际状态并人工核对。")
      const remoteRevision = target.target_node === nodeFingerprint(connection.audiencePublicKey) ? undefined : remoteSessions.current.get(`${target.target_node}:${target.session_id}`)?.revision
      const request = { operation_id: newId(), space_id: spaceId, conversation_id: id, task_id: task.task_id, target_node: target.target_node, session_id: target.session_id, expected_revision: remoteRevision ?? target.revision ?? 0, basis_heads: context.heads, resources: action.type === "open" ? [action.cwd] : action.type === "send" && session ? [session.cwd] : [], action }
      if (action.type === "open") {
        if (["accepted", "cancelled"].includes(task.status)) throw new Error("工单已结束，不能打开新的执行会话。")
        if (session || target.revision || bindingsRef.current.some(item => item.request.conversation_id === id && (item.request.task_id === task.task_id || item.request.target_node === target.target_node))) throw new Error("此工单或设备已有会话或待核对绑定，请先核对原请求。")
        const pendingBinding: PendingBinding = { request: { ...request, action }, task, attempt: null, ...(remote ? { remote: currentRemoteTrust(remote) } : {}) }
        replaceBindings([...bindingsRef.current, pendingBinding])
        await completeOpen(pendingBinding)
      } else {
        if (!session || session.task_id !== task.task_id) throw new Error("执行会话与所选工单不一致。")
        const scoped = await executionScope(task, request, { type: "open", executor: session.executor, cwd: session.cwd }, session.binding, remote)
        await sendOperation({ ...request, binding: scoped.params.binding }, undefined, scoped.facts.remote?.authorization)
      }
    }),
    resumeBinding: (operationId: string) => {
      const original = bindingsRef.current.find(item => item.request.operation_id === operationId)
      if (!original) return Promise.reject(new Error("原绑定记录已变化，请重新核对。"))
      return withMutation(`operate:${original.request.target_node}:${original.request.session_id}`, async () => {
        requireContext(original.request.conversation_id)
        await readConversation(original.request.conversation_id)
        await completeOpen(original)
      })
    },
    reconcile: async (operation: PendingOperation): Promise<ReconcileRecord> => {
      requireCurrent()
      const original = pendingRef.current.find(item => item.operation_id === operation.operation_id)
      if (!original || !same(original, operation)) throw new Error("原待核对记录已变化，请重新读取并核对。")
      const scope = original.remote ? projectMetadataScope(original.request) : await operationScope(original.request)
      const method = connection.authorization.actions.includes("operation_reconcile") ? "reconcile" : "operation"
      const response = record(await connection.rpc(method, { ...scope.params, operation_id: original.operation_id }, scope.facts))
      const result = parseReconcile(response)
      requireCurrent()
      if (result.operation && !same(record(result.operation.raw).request, original.request)) throw new Error("执行设备返回的完整请求与原操作不同，已保留待核对记录。")
      if (original.remote) {
        if (response.actual !== null && response.actual !== undefined) {
          const observation = await verifyRemoteObservation(original.request, record(response.actual), original.remote, original.attempt ?? undefined)
          if (original.attempt && result.operation && result.operation.state === observation.state && ['succeeded', 'failed', 'rejected'].includes(observation.state)) {
            if (writer.current) {
              removePending(original.operation_id)
              replaceBindings(bindingsRef.current.filter(item => item.request.operation_id !== original.operation_id))
              if (activeId.current === original.conversation_id) await refreshRef.current()
            }
            setNoticeText("B 和 C 的终态一致，且 C 完整结果精确匹配原签名。已核对原操作，没有重新执行。")
            return { ...result, verification: 'execution_result' }
          }
          setNoticeText(`已独立核验 C 的完整签名结果（${stateNames[observation.state] ?? observation.state}）。此次核对只读，原操作仍需明确确认后处理。`)
          return { ...result, verification: "execution_receipt" }
        }
        setNoticeText("已读取 B 项目的持久执行记录。尚未独立核实 C 的当前结果，原请求继续保留。")
        return { ...result, verification: "project_metadata" }
      }
      if (reconcileDecision(result, original) === "settled" && writer.current) {
        removePending(original.operation_id)
        replaceBindings(bindingsRef.current.filter(item => item.request.operation_id !== original.operation_id))
        if (activeId.current === original.conversation_id) await refreshRef.current()
      }
      return result
    },
    confirmMissing: async (operationId: string) => {
      requireCurrent()
      const operation = pendingRef.current.find((item) => item.operation_id === operationId)
      if (!operation || operation.conversation_id !== activeId.current) throw new Error("请先回到原操作的会话。")
      return withMutation(`operate:${operation.target_node}:${operation.session_id}`, async () => {
        if (nodeFingerprint(connection.audiencePublicKey) !== operation.target_node) throw new Error("请连接原执行设备，再读取完整会话并核对原操作。")
        const scope = await operationScope(operation.request)
        const result = parseReconcile(await connection.rpc("reconcile", { ...scope.params, operation_id: operation.operation_id }, scope.facts))
        requireCurrent()
        if (result.operation && !same(record(result.operation.raw).request, operation.request)) throw new Error("执行设备返回的完整请求与原操作不同，已保留待核对记录。")
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
      const context = requireContext(id), task = context.tasks.find(task => task.task_id === item.task_id)
      if (!task) throw new Error("执行会话缺少对应工单。")
      const remote = item.target_node !== nodeFingerprint(connection.audiencePublicKey)
      if (remote) {
        remoteSessions.current.delete(`${item.target_node}:${item.session_id}`)
        setActual(previous => { const next = { ...previous }; delete next[`${item.target_node}:${item.session_id}`]; return next })
      }
      const original = remote ? [...context.operations].reverse().find(operation => operation.request.target_node === item.target_node && operation.request.session_id === item.session_id && operation.execution_source) : undefined
      if (remote && !original) throw new Error("B 项目尚未提供此会话的签名执行历史。当前仅显示 B 持久记录，尚未核实 C。")
      const scope = original ? projectMetadataScope(original.request) : await executionScope(task, { ...item, conversation_id: id }, { type: "open", executor: item.executor, cwd: item.cwd }, item.binding)
      const result = record(await connection.rpc("session", scope.params, scope.facts))
      requireCurrent()
      if (activeId.current !== id) return
      if (remote) {
        if (result.actual === null || result.actual === undefined) {
          setNoticeText("已读取 B 项目记录。C 当前不可核实，原执行状态和请求继续保留。")
          return
        }
        if (!original) throw new Error("B 未返回原操作签名历史。")
        const ownAttempt = pendingRef.current.find(pending => pending.operation_id === original.request.operation_id)?.attempt
        await verifyRemoteObservation(original.request, record(result.actual), undefined, ownAttempt ?? undefined)
        setNoticeText("已独立核验 C 的完整签名结果及当前可用的会话快照。")
        return
      }
      const parsed = parseSession(result.session)
      if (parsed.target_node !== item.target_node || parsed.session_id !== item.session_id || parsed.task_id !== task.task_id || parsed.executor !== item.executor || parsed.cwd !== item.cwd || parsed.authority_public_key !== connection.audiencePublicKey || !same(parsed.task_creation, task.task_creation) || !same(parsed.binding, scope.params.binding)) throw new Error("执行设备返回了其他会话或工单绑定。")
      setActual((previous) => ({ ...previous, [`${item.target_node}:${item.session_id}`]: result.actual }))
    },
    clearActual: (item: SessionRecord) => { setActual((previous) => { const next = { ...previous }; delete next[`${item.target_node}:${item.session_id}`]; return next }) },
    resolveUnknown: (targetNode: string, operationId: string, outcome: string, evidence: string) => withMutation(`resolve:${operationId}`, async () => {
      const id = activeId.current
      const context = requireContext(id)
      if (evidence.trim().length < 12 || !["succeeded", "failed", "rejected"].includes(outcome)) throw new Error("请填写本人核查证据与明确结果。")
      if (resolutionsRef.current.some(item => item.request.operation_id === operationId)) throw new Error("此操作已有待核对的结果处理请求。请核对并重试原签名请求。")
      const pendingOperation = pendingRef.current.find(item => item.operation_id === operationId && item.target_node === targetNode && item.conversation_id === id)
      const recorded = context.operations.find(item => item.request.operation_id === operationId && item.request.target_node === targetNode)
      if (recorded && recorded.state !== "unknown") throw new Error("原操作已不处于结果未知状态，请重新核对。")
      if (pendingOperation && recorded && !same(pendingOperation.request, record(recorded.raw).request)) throw new Error("执行设备返回的完整请求与原操作不同，已保留待核对记录。")
      const request = pendingOperation?.request ?? recorded?.request
      if (!request) throw new Error("未找到原操作的完整请求，请先回到原会话读取并核对。")
      if (request.target_node !== nodeFingerprint(connection.audiencePublicKey)) {
        const task = context.tasks.find(item => item.task_id === request.task_id)
        if (!task) throw new Error("原操作缺少已验证的任务。")
        const { taskRef } = verifyTask(task, id)
        const remote = remoteForSession(targetNode, request.session_id, taskRef, 'resolution')
        if (!remote) throw new Error("原操作缺少独立的结果处理授权。")
        let resolver: Awaited<ReturnType<typeof openRemoteResolver>> | undefined
        let pendingResolution: PendingRemoteResolution
        try {
          const metadata = projectMetadataScope(request)
          let response = record(await connection.rpc("operation", metadata.params, metadata.facts))
          requireContext(id)
          let operation = parseOperation(response.operation)
          if (!same(record(operation.raw).request, request) || operation.state !== 'unknown') throw new Error("B 的原操作或未知状态已变化，请重新核对。")
          const originalHandoff = response.original_handoff == null ? undefined : record(response.original_handoff)
          if (response.actual === null || response.actual === undefined) {
            if (!originalHandoff) throw new Error("B 尚未提供可供独立观察的完整原签名转交，已保留未知操作。")
            const observer = await openRemoteResolver(request, currentRemoteTrust(remote))
            try { response = record(await connection.observeRemoteExecution(originalHandoff, currentRemoteTrust(remote), observer.rawContext)) }
            finally { observer.handle.release() }
            requireContext(id)
            operation = parseOperation(response.operation)
            if (!same(record(operation.raw).request, request) || operation.state !== 'unknown' || !same(response.original_handoff, originalHandoff)) throw new Error("观察响应改变了原操作或签名转交，已保留待核对记录。")
          }
          const actual = response.actual ?? observationsRef.current.get(operationId)?.actual ?? record(response.operation).result
          if (actual === null || actual === undefined) throw new Error("C 尚未返回可独立验证的原执行回执，已保留原操作。")
          resolver = await openRemoteResolver(request, currentRemoteTrust(remote))
          requireContext(id)
          const original = pendingOperation?.attempt
          const observation = original ? await connection.remoteExecutionResultHistory(currentRemoteTrust(remote), record(actual), original, request.binding) : resolver.handle.observation(record(actual), request.binding)
          requireContext(id)
          if (observation.operationId !== operationId || originalHandoff && !same(observation.actual.handoff, originalHandoff)) throw new Error("C 回执属于其他操作或签名转交，已保留原请求。")
          observationsRef.current.set(operationId, observation)
          setRemoteObservations(previous => ({ ...previous, [operationId]: observation }))
          if (['succeeded', 'failed', 'rejected'].includes(observation.state) && observation.state !== outcome) throw new Error(`C 的已核验终态为${stateNames[observation.state] ?? observation.state}。请重新核查，并按这一真实结果确认处理。`)
          const authorizationId = newId()
          const intent = resolver.handle.resolution(observation.actual, authorizationId, outcome, evidence.trim())
          pendingResolution = { request, remote: currentRemoteTrust(remote), observation: observation.actual, body: intent.body, context: intent.context, attempt: null }
          replaceRemoteResolutions([...resolutionsRef.current, pendingResolution])
        } finally { resolver?.handle.release() }
        await sendRemoteResolution(pendingResolution)
        return
      }
      if (outcome === 'rejected') throw new Error("本机未知操作只接受已完成或未完成的核查结果。")
      const scope = await operationScope(request)
      await connection.rpc("resolve_unknown", { ...scope.params, operation_id: operationId, outcome, evidence }, scope.facts)
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    resumeRemoteResolution: (operationId: string) => withStorageMutation(`resolve:${operationId}`, async () => {
      const pending = resolutionsRef.current.find(item => item.request.operation_id === operationId)
      if (!pending || pending.request.conversation_id !== activeId.current) throw new Error("请回到原会话核对待确认的结果处理请求。")
      await sendRemoteResolution(pending)
    }),
    ack: async (notificationId: string) => { requireWriter(); await connection.rpc("ack", { notification_id: notificationId }); requireCurrent(); await refreshRef.current() },
    openNotification: async (notification: NotificationRecord) => { selectConversation(notification.conversation_id); setView(["approval", "needs_input"].includes(notification.kind) ? "sessions" : "chat"); await refreshRef.current() },
    enableBrowserNotifications: async () => {
      requireCurrent()
      if (!canReadNotifications) throw new Error("当前项目授权不允许读取通知。")
      if (!("Notification" in window)) return
      const permission = Notification.permission === "granted" ? "granted" : await Notification.requestPermission()
      requireCurrent()
      writeStorage(scoped("notify"), permission === "granted")
      if (permission === "granted") { writeStorage(scoped("seen"), notificationsRef.current.map((item) => item.notification_id)); setNoticeText("浏览器提醒已开启。已有通知保留在通知列表中。") }
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
