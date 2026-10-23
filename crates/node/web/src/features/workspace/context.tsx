"use client"

import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import type { ReadyConnection } from "@/lib/node"
import { attestationPayload } from "../../../generated/vibeface_secure.js"
import { signClient } from "../../../transport.js"
import { messageOf } from "@/lib/node"
import { parseLogs, reportLog, type LogFilter } from "@/lib/web-log"
import { enqueue, indexedOutbox, settle, type OutboxItem, type OutboxStore } from "@/lib/outbox"
import type { Tabs } from "@/features/identity/context"
import { mergeProgress, type LiveProgress } from "@/lib/progress"
import { isQueuedReceipt, prepareSend } from "../chat/protocol"
import {
  inputText, parseConversation, parseNodes, parseNotifications,
  parseBot, parseBots, parseCreateBot, parsePrimary, parsePendingBot, record,
  parseArtifactPage, parseBinaryPage, parseWriteStatus, parseQuestions, parseAnswer, parseOpenWorks, parseRecords, attachmentName, signedOrder,
} from "./model"
import type { Attachment } from "../chat/types"
import { mergeBots, mergeConversation, mergeNotifications, mergeOpenWorks, mergeQuestions } from "./records"
import { answerBody, answerId, answerScope } from "./questions"
import { networkKey, newId, readStorage, writeStorage } from "./storage"
import type {
  AnswerResult, ConversationPayload, NodeInfo, NotificationRecord, PendingAnswer, Question,
  PendingMessage, WorkspaceView, BotRecord, CreateBotRequest, PendingBot, BotProject, PendingCheck, WriteState, WriteStatus, OpenWork, ScheduleAction,
} from "./types"

/** Why this device does not write now (docs/design/unified-network.md 5.7 and 5.8). */
const writeNotices: Record<Exclude<WriteState, "writable">, string> = {
  aligning: "正在和你的其他设备同步，稍后就能发送。",
  read_only: "这台设备暂时只读。去待处理确认一下就能发送。",
  forked: "这台设备的数据被复制过，已暂停发送。去待处理看看。",
}

export type InterruptedReply = { trigger: string; conversation_id: string; after?: string; text: string }

/** Signed writes this origin has not seen confirmed, shared by every tab (docs/design/unified-network.md 4.5). */
const browserOutbox = indexedOutbox()
const soleTab: Tabs = { leader: () => true, post() {}, listen: () => () => {} }
const responded = (caught: unknown) => caught instanceof Error && "responded" in caught && caught.responded === true

function useWorkspaceState(connection: ReadyConnection, openConversation: (conversationId: string) => void = () => {}, tabs: Tabs = soleTab) {
  const networkId = connection.principal.network_id
  const clientId = connection.principal.device_id
  /** A local session reads this computer's records, but the network takes only a member's signature. */
  const joined = connection.principal.source === "membership"
  const prefix = `${networkId}.${clientId}:`
  const outbox: OutboxStore = {
    all: async () => (await browserOutbox.all()).filter((item) => item.key.startsWith(prefix)),
    put: (item) => browserOutbox.put(item),
    delete: (key) => browserOutbox.delete(key),
  }
  const outboxKey = (kind: string, id: string) => `${prefix}${kind}:${id}`
  /** This device's signature over the exact event body the Node will write for it. */
  const attest = async (scope: string, body: Record<string, unknown>) =>
    ({ device_id: clientId, signature: await signClient("attestation", attestationPayload(networkId, scope, JSON.stringify(body))) })
  const [booted, setBooted] = useState(false)
  const [error, setError] = useState<{ text: string; retry?: () => void } | null>(null)
  const [noticeText, setNoticeText] = useState("")
  const [view, setView] = useState<WorkspaceView>("other")
  // Read synchronously: the chat route selects its conversation in the same commit that sets the view.
  const viewRef = useRef<WorkspaceView>("other")
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [writeState, setWriteState] = useState<WriteStatus>({ state: "writable", conflicts: [] })
  const [conversationId, setConversationId] = useState("")
  const [data, setData] = useState<ConversationPayload | null>(null)
  const [notifications, setNotifications] = useState<NotificationRecord[]>([])
  const [questions, setQuestions] = useState<Question[]>([])
  const [openWorks, setOpenWorks] = useState<OpenWork[]>([])
  const openWorksRef = useRef<OpenWork[]>([])
  /** Answers this browser saw land, by question, until the network's own reads show them. */
  const [answered, setAnswered] = useState<Readonly<Record<string, string>>>({})
  const [pendingAnswers, setPendingAnswers] = useState<PendingAnswer[]>([])
  const answersRef = useRef<PendingAnswer[]>([])
  const [pendingMessages, setPendingMessages] = useState<PendingMessage[]>([])
  const [bots, setBots] = useState<BotRecord[]>([])
  const [primary, setPrimary] = useState<string | null>(null)
  const [pendingBot, setPendingBot] = useState<PendingBot | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const [messageBusy, setMessageBusy] = useState(false)
  const [liveProgress, setLiveProgress] = useState<LiveProgress[]>([])
  const [interrupted, setInterrupted] = useState<InterruptedReply[]>([])
  const liveRef = useRef<LiveProgress[]>([])
  liveRef.current = liveProgress
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
  const requireCurrent = () => { if (!current()) throw new Error("连接换了，请再查一次。") }
  const requireContext = (id = activeId.current) => {
    requireCurrent()
    const context = dataRef.current
    if (readConnection.current !== connection || !id || id !== activeId.current || context?.conversation?.conversation_id !== id) throw new Error("先选择并读取对话。")
    return context
  }
  const requireBotContext = (id = activeId.current) => {
    if (!bots.some((bot) => bot.conversation_id === id)) throw new Error("请先选一个 Bot。")
    return requireContext(id)
  }
  const showError = (caught: unknown, action?: string, retry?: () => void) => { if (current()) setError({ text: messageOf(caught, action), retry }) }
  /** A failed background read names itself and offers one more try. */
  const showRefreshError = (caught: unknown) => showError(caught, "读取最新内容", () => { void refreshRef.current().catch(showRefreshError) })
  const requireWritable = () => {
    requireCurrent()
    if (!joined) throw new Error("本机管理不能发送。用加入链接加入后再操作。")
    if (!booted) throw new Error("本地记录还没读完，先暂停发送。")
    if (readConnection.current !== connection) throw new Error("正在补读当前设备的记录，已暂停写入。")
    if (writeState.state !== "writable") throw new Error(writeNotices[writeState.state])
  }
  const scoped = (key: string) => networkKey(`${key}.${clientId}`, networkId)
  /** Unconfirmed messages and answers as the page shows them, read back from the outbox. */
  const syncPending = async () => {
    const items = await outbox.all()
    const next: PendingMessage[] = items.filter((item) => item.method === "message").map((item) => ({
      message_id: String(item.params.message_id), conversation_id: String(item.params.conversation_id), text: String(item.params.text),
      report_results: item.params.report_results !== false, ...(item.rejected === undefined ? {} : { rejected: item.rejected }),
    }))
    const answers: PendingAnswer[] = items.filter((item) => item.method === "answer" && item.rejected === undefined)
      .map((item) => ({ question_id: String(item.params.question_id), text: String(item.params.text) }))
    messagesRef.current = next
    answersRef.current = answers
    if (alive.current) { setPendingMessages(next); setPendingAnswers(answers) }
  }
  const outboxChanged = async () => { await syncPending(); tabs.post({ type: "outbox", networkId }) }
  /** Keeps the signed request until the entry answers; an unknown outcome stays for the next check. */
  const signedWrite = async (kind: string, id: string, method: string, params: Record<string, unknown>) => {
    const item = await enqueue(outbox, { key: outboxKey(kind, id), method, params, created_at_ms: Date.now() })
    try {
      const result = await connection.rpc(item.method, item.params)
      await outbox.delete(item.key)
      return result
    } catch (caught) {
      if (responded(caught)) await outbox.delete(item.key)
      throw caught
    }
  }
  const selectConversation = (id: string) => {
    requireCurrent()
    activeId.current = id
    readSequence.current += 1
    dataRef.current = null
    setConversationId(id)
    setData(null)
    setLiveProgress([])
    setInterrupted([])
  }
  const readConversation = async (id: string) => {
    const sequence = ++readSequence.current
    const next = parseConversation(await connection.rpc("conversation", { conversation_id: id }), id)
    requireCurrent()
    if (activeId.current !== id) throw new Error("对话有更新，请再查一次。")
    // A newer read of this conversation shows its own result; this one is still a fresh read for the caller.
    if (sequence !== readSequence.current) return next
    return showConversation(id, next)
  }
  const showConversation = async (id: string, next: ConversationPayload) => {
    // While the entry catches up, what the page already showed stays on screen.
    const shown = dataRef.current
    if (connection.catchingUp?.() && shown?.conversation?.conversation_id === id) {
      const missing = shown.messages.filter((message) => !next.messages.some((item) => item.message_id === message.message_id))
      if (missing.length) next.messages = [...next.messages, ...missing].sort((a, b) => a.created_at_ms - b.created_at_ms)
    }
    dataRef.current = next
    setData(next)
    // A message or an answer settles once the Node's own history holds it.
    const admitted = (item: PendingMessage) => item.conversation_id === id && next.messages.some((message) => message.message_id === item.message_id)
    const landed = answersRef.current.filter((item) => next.messages.some((message) => message.message_id === answerId(item.question_id)))
    if (current() && (messagesRef.current.some(admitted) || landed.length)) {
      for (const item of messagesRef.current.filter(admitted)) await outbox.delete(outboxKey("message", item.message_id))
      for (const item of landed) await outbox.delete(outboxKey("answer", item.question_id))
      await outboxChanged()
    }
    // Streamed text of a reply that ended without a final message is kept locally and shown as interrupted.
    const ended = (job: { state: string; reply_message_id?: string } | undefined) => job && ["completed", "failed", "cancelled", "interrupted", "rejected"].includes(job.state) && !(job.reply_message_id && next.messages.some((message) => message.message_id === job.reply_message_id))
    const kept = readStorage<InterruptedReply[]>(scoped("interrupted"), []).filter((item) => item.conversation_id !== id || next.scheduler?.jobs.some((job) => job.trigger === item.trigger && ended(job)))
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
      return !job || (!job.reply_message_id || !next.messages.some((message) => message.message_id === job.reply_message_id)) && !["completed", "failed", "cancelled", "interrupted", "rejected"].includes(job.state)
    }))
    if (joined && current() && readConnection.current === connection && viewRef.current === "chat" && !document.hidden) {
      const seen = notificationsRef.current.filter((item) => item.conversation_id === id)
      for (const notification of seen) {
        try {
          await signedWrite("ack", notification.notification_id, "ack", { notification_id: notification.notification_id, attestation: await attest(notification.conversation_id, { type: "notification_acked", notification_id: notification.notification_id, client_id: clientId }) })
          requireCurrent()
          notificationsRef.current = notificationsRef.current.filter((item) => item.notification_id !== notification.notification_id)
          setNotifications(notificationsRef.current)
        } catch (caught) { showError(caught, "标记已读"); break }
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
        const popup = new Notification(item.kind === "question" ? "Vibeface · 需要你回答" : "Vibeface · 进展", { body: item.text, tag: item.notification_id })
        popup.onclick = () => {
          if (!current()) { popup.close(); return }
          window.focus(); openConversation(item.conversation_id)
          popup.close()
        }
      } catch { /* The persisted inbox remains available when browser notifications fail. */ }
    }
    writeStorage(networkKey("seen", networkId), [...seen].slice(-1000))
  }
  /** A device starts with the history it found read: what waited on its first full read never counts as unread here. */
  const unreadSince = (items: NotificationRecord[], first = false) => {
    const key = networkKey(`read-before.${clientId}`, networkId)
    const before = readStorage<string[] | null>(key, null)
    if (!before && first) { writeStorage(key, items.map((item) => item.notification_id)); return [] }
    return before ? items.filter((item) => !before.includes(item.notification_id)) : items
  }
  const scheduleRefresh = () => {
    if (!current() || refreshTimer.current !== null) return
    refreshTimer.current = window.setTimeout(() => {
      refreshTimer.current = null
      void refreshRef.current().catch(showRefreshError)
    }, 350)
  }
  const refresh = async () => {
    requireCurrent()
    if (refreshingRef.current && refreshingConnection.current === connection) { refreshAgain.current = true; return }
    refreshingRef.current = true
    refreshingConnection.current = connection
    setRefreshing(true)
    try {
      const [rawNodes, rawNotifications, rawBots, rawWrite, rawQuestions, rawWorks] = await Promise.all([
        connection.rpc("nodes"), connection.rpc("notifications"), connection.rpc("bots"), connection.request("write_state"), connection.rpc("questions"), connection.rpc("open_works"),
      ])
      requireCurrent()
      const nextNodes = parseNodes(rawNodes), nextNotifications = unreadSince(parseNotifications(rawNotifications), true)
      const nextBots = parseBots(rawBots), nextQuestions = parseQuestions(rawQuestions)
      openWorksRef.current = parseOpenWorks(rawWorks)
      setNodes(nextNodes); setNotifications(nextNotifications); setBots(nextBots); setPrimary(parsePrimary(rawBots)); setWriteState(parseWriteStatus(rawWrite)); setQuestions(nextQuestions); setOpenWorks(openWorksRef.current)
      if (botCreationRef.current && nextBots.some((item) => item.bot_id === botCreationRef.current?.bot_id)) clearPendingBot()
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
  /** One pushed change merged in place; a full read only when a read races it or it does not parse. */
  const applyRecords = async (value: Record<string, unknown>) => {
    const parsed = parseRecords(value), records = { ...parsed, notifications: unreadSince(parsed.notifications) }, id = records.conversation_id
    if (refreshingRef.current) scheduleRefresh()
    setQuestions((previous) => mergeQuestions(previous, records))
    setBots((previous) => mergeBots(previous, records))
    openWorksRef.current = mergeOpenWorks(openWorksRef.current, records)
    setOpenWorks(openWorksRef.current)
    const fresh = records.notifications.filter((item) => !notificationsRef.current.some((known) => known.notification_id === item.notification_id))
    notificationsRef.current = mergeNotifications(notificationsRef.current, records)
    setNotifications(notificationsRef.current)
    notifyBrowser(fresh)
    const shown = dataRef.current
    if (id === activeId.current && readConnection.current === connection && shown?.conversation?.conversation_id === id) await showConversation(id, mergeConversation(shown, records))
  }
  /** A change the Node pushed without its records: the bot list, and the conversation if it is open. */
  const reloadScope = async (id: string) => {
    const rawBots = await connection.rpc("bots")
    requireCurrent()
    setBots(parseBots(rawBots)); setPrimary(parsePrimary(rawBots))
    if (id === activeId.current && readConnection.current === connection) await readConversation(id)
  }

  const loadPending = async () => {
    const rawBot = readStorage<unknown>(scoped("pending-bot"), null, true)
    const storedBot = rawBot == null ? null : parsePendingBot(rawBot)
    botCreationRef.current = storedBot; setPendingBot(storedBot)
    await syncPending()
  }
  /** Is this write already in the entry's history (or moot)? Asked by business ID before any resubmission. */
  const recorded = async (item: OutboxItem) => {
    if (item.method === "ack") return !parseNotifications(await connection.rpc("notifications")).some((note) => note.notification_id === item.params.notification_id)
    if (item.method === "create_bot") return parseBots(await connection.rpc("bots")).some((bot) => bot.bot_id === item.params.bot_id)
    if (item.method === "delete_bot") return !parseBots(await connection.rpc("bots")).some((bot) => bot.conversation_id === item.params.conversation_id)
    // Every unanswered question is listed, so one missing from the list has its answer.
    if (item.method === "set_primary_bot") return parsePrimary(await connection.rpc("bots")) === item.params.conversation_id
    if (item.method === "answer") return !parseQuestions(await connection.rpc("questions")).some((question) => question.question_id === item.params.question_id)
    const id = String(item.params.conversation_id)
    const context = parseConversation(await connection.rpc("conversation", { conversation_id: id }), id)
    if (item.method === "message") return context.messages.some((message) => message.message_id === item.params.message_id)
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
    return true
  }
  /** After a (re)connection: confirm what the entry has, resubmit the rest byte for byte, stop at an unknown. */
  const settleOutbox = async () => {
    if (!tabs.leader() || !current()) return
    const result = await settle(outbox, {
      recorded: async (item) => { requireCurrent(); return recorded(item) },
      submit: async (item) => { requireCurrent(); await connection.rpc(item.method, item.params) },
    })
    if (!alive.current) return
    await outboxChanged()
    if (result.confirmed.length || result.resubmitted.length || result.rejected.length) scheduleRefresh()
  }

  useEffect(() => {
    alive.current = true
    inFlight.current = new Set()
    setMessageBusy(false); setLiveProgress([]); setError(null); setNoticeText("")
    try {
      const url = new URL(window.location.href)
      const routeBot = /^\/b\/([^/]+)/.exec(url.pathname)?.[1]
      if (routeBot) writeStorage(scoped("selected-bot"), decodeURIComponent(routeBot))
      activeId.current = ""
      setConversationId(activeId.current)
      void loadPending().then(() => { if (alive.current) setBooted(true) }, (caught) => showError(caught, "读取待发消息"))
    } catch (caught) { showError(caught) }
    const stop = tabs.listen((message) => { if (message.type === "outbox" && message.networkId === networkId) void syncPending().catch(() => {}) })
    return () => { alive.current = false; stop() }
  }, [connection])

  useEffect(() => {
    if (!booted) return
    void refreshRef.current().then(() => settleOutbox()).catch(showRefreshError)
    const unsubscribe = connection.subscribe((event) => {
      if (!current()) return
      if (event.type === "records") { applyRecords(event.value).catch(() => scheduleRefresh()); return }
      if (event.type === "invalidate") {
        if (event.conversation_id === undefined) scheduleRefresh()
        else reloadScope(event.conversation_id).catch(() => scheduleRefresh())
        return
      }
      if (event.conversation_id !== activeId.current) return
      setLiveProgress((previous) => {
        const next = mergeProgress(previous, event)
        if (next.some((item) => item.needsRefresh && !previous.find((prior) => prior.key === item.key)?.needsRefresh)) scheduleRefresh()
        return next
      })
    })
    // Pushes carry every change; this catches a device going offline, which nothing pushes, and anything missed.
    let ticks = 0
    const timer = window.setInterval(() => {
      ticks += 1
      if (ticks % 20 === 0) scheduleRefresh()
      else if (openWorksRef.current.length) connection.rpc("nodes").then((raw) => { if (current()) setNodes(parseNodes(raw)) }, () => {})
    }, 15000)
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
    if (mutations.has(key)) throw new Error("上一个操作还没结束，请稍等。")
    mutations.add(key)
    try { return await work() } finally { mutations.delete(key) }
  }
  const clearMessages = () => { if (current()) { setError(null); setNoticeText("") } }
  const run = async (action: () => Promise<void>, label?: string) => {
    clearMessages()
    if (label) reportLog({ level: "info", event: "web.action", message: label })
    try { await action() } catch (caught) {
      reportLog({ level: "warn", event: "web.action_failed", message: messageOf(caught), fields: { action: label } })
      showError(caught, label)
    }
  }
  const requireControlRefresh = () => {
    if (!current()) return
    readConnection.current = null
    readSequence.current += 1
    setNoticeText("还没确认结果，正在重新查。")
    scheduleRefresh()
  }
  /** A retry, here or through another entrance, sends the same signed request; nothing is re-signed. */
  /** Sends one file to the entry in 1 MiB chunks; an interrupted upload continues where the entry's disk stopped. */
  const uploadAttachment = async (id: string, file: File): Promise<Attachment> => {
    const bytes = new Uint8Array(await file.arrayBuffer())
    if (!bytes.length || bytes.length > 64 * 1024 * 1024) throw new Error(`附件「${file.name}」需在 64 MiB 以内且不能为空。`)
    const hash = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))].map((byte) => byte.toString(16).padStart(2, "0")).join("")
    let offset = 0
    for (;;) {
      requireCurrent()
      const chunk = bytes.subarray(offset, offset + 1024 * 1024)
      let binary = ""
      for (let index = 0; index < chunk.length; index += 0x8000) binary += String.fromCharCode(...chunk.subarray(index, index + 0x8000))
      const reply = record(await connection.rpc("upload_attachment", { conversation_id: id, hash, size: bytes.length, offset, data: btoa(binary) }))
      if (reply.complete === true) return { hash, size: bytes.length, name: attachmentName(file.name) }
      offset = Number(reply.offset)
    }
  }
  const submitMessage = async (text: string, files: File[] = []) => {
    requireContext()
    const id = activeId.current
    const prior = messagesRef.current.find((item) => item.conversation_id === id) ?? null
    const prepared = prepareSend({ conversationId: id, text, pending: prior, reportResults: true })
    if (!prepared.ok) throw new Error(prepared.error)
    const key = outboxKey("message", prepared.pending.message_id)
    let item = prior ? (await outbox.all()).find((entry) => entry.key === key) : undefined
    if (!item) {
      const { body } = prepared
      const uploaded: Attachment[] = []
      for (const file of files) uploaded.push(await uploadAttachment(id, file))
      const attachments = signedOrder(uploaded)
      const extra = attachments.length ? { attachments } : {}
      const attestation = await attest(body.conversation_id, { type: "message_posted", message_id: body.message_id, role: "user", text: body.text, ...extra, report_results: body.report_results })
      item = await enqueue(outbox, { key, method: "message", params: { ...body, ...extra, attestation }, created_at_ms: Date.now() })
    } else if (item.rejected !== undefined) {
      item = { ...item, rejected: undefined }
      await outbox.put(item)
    }
    requireWritable()
    await outboxChanged()
    try {
      const receipt = await connection.rpc("message", item.params)
      requireCurrent()
      if (!isQueuedReceipt(receipt, prepared.pending)) {
        if (activeId.current === id) await refreshRef.current().catch(showRefreshError)
        return false
      }
      await outbox.delete(key)
      await outboxChanged()
      setNoticeText("")
      if (activeId.current === id) await refreshRef.current().catch(showRefreshError)
      return true
    } catch (caught) {
      if (current() && responded(caught)) {
        await outbox.put({ ...item, rejected: caught instanceof Error ? caught.message : String(caught) })
        await outboxChanged()
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
    if (pending.submission?.state !== "rejected" || pending.submission.entry_node !== connection.entry.device_id) throw new Error("请回到原来的设备确认 Bot 是否建好，内容已保留。")
    const known = parseBots(await connection.rpc("bots"))
    requireCurrent()
    const match = known.find((bot) => bot.bot_id === pending.bot_id || bot.conversation_id === pending.conversation_id)
    if (match && (match.bot_id !== pending.bot_id || match.conversation_id !== pending.conversation_id || match.name !== pending.name || match.instructions !== pending.instructions || !sameProject(match.project, pending.project))) throw new Error("找到了这个 Bot，请先确认它是否已经建好。")
    clearPendingBot()
    if (!match) setNoticeText("确认没建成，可以修改后再试。")
    return match ?? null
  }
  const selectedBot = bots.find((bot) => bot.conversation_id === conversationId) ?? null
  const selectBot = async (bot: BotRecord) => {
    requireCurrent()
    writeStorage(scoped("selected-bot"), bot.bot_id)
    selectConversation(bot.conversation_id)
    await refreshRef.current()
  }
  const sendMessage = (input: string, files: File[] = []) => withMutation(`message:${activeId.current}`, async () => {
    if (!bots.some((bot) => bot.conversation_id === activeId.current)) throw new Error("请先选一个 Bot。")
    const text = inputText(input, "消息内容", 65536)
    setMessageBusy(true)
    try { return await submitMessage(text, files) } finally { if (current()) setMessageBusy(false) }
  })
  const incomplete = !data
  const messagePending = pendingMessages.find((item) => item.conversation_id === conversationId) ?? null
  const chatSlot = { conversationId: conversationId || null }

  return {
    info: connection.info, generation: connection.generation, isCurrent: current, connectionOnline: current(), connectionText: current() ? "已建立端到端加密连接" : "连接已断开，正在恢复只读订阅；未确认操作不会自动重发。",
    error, noticeText, view, setView: (next: WorkspaceView) => { viewRef.current = next; setView(next); clearMessages(); if (next === "chat") scheduleRefresh() }, nodes, conversationId, data, notifications, questions, openWorks, answered, pendingAnswers, bots, primary, selectedBot, pendingBot,
    refreshing, incomplete, chatSlot, messagePending, messageBusy, liveProgress, interrupted,
    joined,
    writable: joined && booted && current() && readConnection.current === connection && writeState.state === "writable",
    writeNotice: readConnection.current !== connection ? "正在补读当前设备的记录，已暂停写入。" : "",
    writeState, readOnlyNotice: writeState.state === "writable" ? "" : writeNotices[writeState.state],
    diagnostics: (sinceMs: number) => { requireCurrent(); return connection.request("diagnostics", { since_ms: sinceMs }) },
    logs: async (filter: LogFilter) => { requireCurrent(); return parseLogs(await connection.request("logs", filter)) },
    confirmWritable: async () => { requireCurrent(); setWriteState(parseWriteStatus(await connection.request("confirm_writable"))); scheduleRefresh() },
    syncText: !data ? "发一条消息开始对话。" : "已加载这台设备上的记录。",
    refresh, run, announce: (notice: string) => { if (current()) setNoticeText(notice) }, notificationButton,
    selectBot,
    createBot: (input: { name: string; instructions: string; project?: BotProject }) => withMutation("create-bot", async () => {
      const name = inputText(input.name, "Bot 名称", 512), instructions = input.instructions.trim()
      if (new TextEncoder().encode(instructions).length > 8192) throw new Error("职责说明不能超过 8192 字节。")
      if (new TextEncoder().encode(JSON.stringify([name, instructions, input.project ?? null])).length > 12000) throw new Error("内容太长了，请缩短名称或说明。")
      const prior = botCreationRef.current
      if (prior && (prior.name !== name || prior.instructions !== instructions || !sameProject(prior.project, input.project))) throw new Error("Bot 还没确认建好，内容已保留，请原样重试。")
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
      const attestation = await attest(request.conversation_id, { type: "bot_created", bot_id: request.bot_id, name: request.name, instructions: request.instructions, ...(request.project ? { project: request.project } : {}) })
      // The entry first asked schedules the conversation, so a retry through any entry writes the same owner.
      const scheduler = pending.submission?.entry_node ?? connection.entry.device_id
      const owner_attestation = await attest(request.conversation_id, { type: "conversation_owner", scheduler_node: scheduler, epoch: 1 })
      try { response = await signedWrite("bot", request.bot_id, "create_bot", { ...request, scheduler_node: scheduler, owner_attestation, attestation }) }
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
      if (bot.bot_id !== request.bot_id || bot.conversation_id !== request.conversation_id || bot.name !== request.name || bot.instructions !== request.instructions || !sameProject(bot.project, request.project)) throw new Error("Bot 创建结果和填写的内容不一致，请检查。")
      clearPendingBot()
      await selectBot(bot)
      return bot
    }),
    sendMessage,
    cancelManager: (trigger: string) => withMutation(`cancel-manager:${trigger}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      const receipt = record(await connection.rpc("cancel_manager", { conversation_id: id, trigger }))
      requireCurrent()
      const job = record(receipt.job)
      if (receipt.publication_pending === true) setNoticeText("本轮已有结果，正在补齐发布；未提交停止请求。")
      else if (receipt.cancel_requested === true && job.state === "cancelled") setNoticeText("已停止回复，已开始的任务会继续。")
      else if (receipt.cancel_requested === true) setNoticeText("已请求停止回复，已开始的任务会继续。")
      else setNoticeText("现在停不了，请再查一次。")
      if (activeId.current === id) await refreshRef.current()
    }),
    makePrimary: (id: string) => withMutation(`primary:${id}`, async () => {
      const attestation = await attest("bots", { type: "primary_bot_set", conversation_id: id })
      try { await signedWrite("primary", id, "set_primary_bot", { conversation_id: id, attestation }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      await refreshRef.current()
    }),
    deleteBot: (id: string) => withMutation(`bot-delete:${id}`, async () => {
      const attestation = await attest(id, { type: "bot_deleted" })
      try { await signedWrite("bot-delete", id, "delete_bot", { conversation_id: id, attestation }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      await reloadScope(id)
    }),
    controlSchedule: (scheduleId: string, action: ScheduleAction) => withMutation(`schedule:${scheduleId}`, async () => {
      const id = activeId.current
      const attestation = await attest(id, { type: "schedule_controlled", schedule_id: scheduleId, action })
      try { await signedWrite("schedule", `${scheduleId}:${action}`, "control_schedule", { conversation_id: id, schedule_id: scheduleId, action, attestation }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    takeOver: () => withMutation(`owner:${activeId.current}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      if (!context.conversation) throw new Error("这台设备上还没有这个对话，可能还在同步。")
      const scheduler = context.local_node, epoch = context.conversation.owner_epoch + 1
      const attestation = await attest(id, { type: "conversation_owner", scheduler_node: scheduler, epoch })
      try { await signedWrite("owner", `${id}:${epoch}`, "set_owner", { conversation_id: id, scheduler_node: scheduler, epoch, attestation }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      setNoticeText(`已换到 ${nodes.find((node) => node.device_id === scheduler)?.name || "这台设备"}`)
      if (activeId.current === id) await refreshRef.current()
    }),
    interruptWork: (workId: string) => withMutation(`work-control:${workId}`, async () => {
      const id = activeId.current, context = requireBotContext(id)
      const work = context.works.find((item) => item.work_id === workId)
      if (!work || work.outcome || !["accepted", "running"].includes(work.state)) throw new Error("这个任务现在停不了，请再查一次。")
      let receipt: Record<string, unknown>
      try { receipt = record(await connection.rpc("interrupt_work", { conversation_id: id, work_id: workId })) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      setNoticeText(receipt.waiting_for_device === true ? "已请求、等待设备确认。" : "已请求停止任务。")
      if (activeId.current === id) await refreshRef.current()
    }),
    /**
     * The person's answer to a question: a message whose ID comes from the question, so any retry is the same write.
     * An unknown outcome stays in the outbox; trying again sends the stored request without signing anew.
     */
    answer: (question: Question, input: string) => withMutation(`answer:${question.question_id}`, async (): Promise<AnswerResult> => {
      const text = inputText(input, "回答", 65536)
      const key = outboxKey("answer", question.question_id)
      let item = (await outbox.all()).find((entry) => entry.key === key)
      if (item?.rejected !== undefined) { await outbox.delete(key); item = undefined }
      if (item && item.params.text !== text) throw new Error("上一个回答还没确认送达，先再试一次。")
      item ??= await enqueue(outbox, {
        key, method: "answer", created_at_ms: Date.now(),
        params: { question_id: question.question_id, text, attestation: await attest(answerScope(question), answerBody(question.question_id, text)) },
      })
      await outboxChanged()
      let result: AnswerResult
      try { result = parseAnswer(await connection.rpc("answer", item.params), question.question_id) }
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
      const text = note.trim()
      if (new TextEncoder().encode(text).length > 4096) throw new Error("备注太长了。")
      const attestation = await attest(id, { type: "work_finished", work_id: check.work_id, outcome: choice, result_text: text })
      try { await signedWrite("resolve", check.check_id, "resolve", { conversation_id: id, check_id: check.check_id, outcome: choice, note: text, attestation }) }
      catch (caught) { requireControlRefresh(); throw caught }
      requireCurrent()
      await refreshRef.current()
    }),
    /** A coordination process still alive: the person confirms it has stopped. */
    confirmStopped: (check: Extract<PendingCheck, { kind: "process" }>) => withMutation(`pending-check:${check.check_id}`, async () => {
      const id = activeId.current
      requireBotContext(id)
      await connection.rpc("resolve", { conversation_id: id, check_id: check.check_id })
      requireCurrent()
      if (activeId.current === id) await refreshRef.current()
    }),
    readArtifactBytes: async (hash: string, offset = 0) => {
      requireCurrent()
      const result = parseBinaryPage(await connection.rpc("read_artifact", { conversation_id: activeId.current, blob_hash: hash, offset, binary: true }), hash, offset)
      requireCurrent()
      return result
    },
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
      if (!original) throw new Error("原消息还在发送，请稍等。")
      if (original.rejected === undefined) throw new Error("只有设备明确返回失败后才能放弃。断线或超时请重新发送或再查一次。")
      const complete = await readConversation(id)
      requireContext(id)
      const still = messagesRef.current.find((item) => item.conversation_id === id)
      if (still?.message_id !== original.message_id || complete.messages.some((item) => item.message_id === original.message_id)) throw new Error("原消息状态有变化，请再查一次。")
      requireWritable()
      await outbox.delete(outboxKey("message", original.message_id))
      await outboxChanged()
      setNoticeText("已放弃这条消息，没有重发。")
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
  if (!value) throw new Error("工作区组件缺少上下文。")
  return value
}
export function WorkspaceProvider({ connection, openConversation, tabs, children }: { connection: ReadyConnection; openConversation: (conversationId: string) => void; tabs: Tabs; children: ReactNode }) {
  const value = useWorkspaceState(connection, openConversation, tabs)
  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>
}
