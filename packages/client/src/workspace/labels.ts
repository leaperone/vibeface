import { asProvider, type MembershipView, type ProviderId } from "../lib/node.ts"
import type { LiveProgress } from "../lib/progress.ts"
import { confirmReason } from "./model.ts"
import { awaitsAnswer, workApproval } from "./questions.ts"
import type { AgentLane, ContextProjection, ManagerJob, MemoryEntry, NodeInfo, OpenWork, OverviewBot, OverviewCoverage, OverviewJob, OverviewTask, PendingCheck, Question, QueueHoldReason, SessionHandoff, WorkRecord, WorkspaceActivity, WorkspaceActivityAttention, WorkspaceActivityFacts, WorkspaceActivityGroup, WorkspaceActivityJob, WorkspaceActivityView, WorkspaceOverview } from "./types.ts"

export const contextStages = {
  compiled: { label: '已编译', detail: '内容已准备，尚未确认 AI 开始执行。', tone: 'info' },
  started: { label: '已发送', detail: 'AI 已确认开始这一轮。', tone: 'info' },
  completed: { label: '本轮完成', detail: '本轮已结束，共享记录仍会保留。', tone: 'success' },
  failed: { label: '本轮失败', detail: '执行失败，已保存的聊天记录仍会保留。', tone: 'danger' },
  cancelled: { label: '已取消', detail: '本轮已取消，不代表此前的内容未发送。', tone: 'neutral' },
  interrupted: { label: '已中断', detail: '本轮已中断，已保存的聊天记录仍会保留。', tone: 'warning' },
  unknown: { label: '结果未知', detail: '先查明执行结果，请勿直接重试。', tone: 'warning' },
} satisfies Record<ContextProjection['stage'], { label: string; detail: string; tone: 'info' | 'success' | 'danger' | 'neutral' | 'warning' }>

export const contextInvalidation = (reason: string) => reason === 'conversation_owner_changed' ? '负责设备已变化' : reason === 'stable_prompt_changed' ? 'Bot 配置已变化' : reason === 'provider_changed' ? 'AI 产品已切换' : reason === 'runtime_configuration_changed' ? 'AI 运行环境已变化' : '原会话已失效'
export function nativeSessionObservation(status: ContextProjection['session_status']) {
  if (!status) return { label: '状态未知', detail: '当前续接状态未知。' }
  if (status.pending_generation) return { label: '下轮重建', detail: '配置从下一轮生效，当前回合继续使用原配置。' }
  return status.resumable
    ? { label: '可尝试续接', detail: '本机保留续接缓存，继续执行时仍需确认。' }
    : { label: '需要重建', detail: '下次执行需要建立会话。' }
}
export const contextCoverageLabels: Record<string, string> = {
  messages_omitted: '消息', works_omitted: '工作', dispatches_omitted: '派发', questions_omitted: '问题',
  attachments_omitted: '附件', memory_omitted: '记忆', checkpoint_omitted: '历史检查点', handoff_omitted: '交接摘要', pending_checks_omitted: '结果待确认',
}
export const contextIndexLabels: Record<string, string> = { skills: 'Skill', schedules: '定时任务', memory: '记忆索引', sessions: '外部会话', sent: '发出的消息', bots: 'Bot' }
export const memoryOrigins: Record<NonNullable<MemoryEntry['metadata']>['origin'], string> = { agent: 'Bot 记录', user: '你记录', review: '记忆整理' }
export const memoryConflictLabel = (reason: string) => reason === 'user_controlled' ? '受用户保护，未自动覆盖' : reason === 'epoch_changed' ? '数据版本已变化，未覆盖现有记忆' : reason === 'revision_changed' ? '记忆已被更新，未覆盖新值' : '提交冲突，未覆盖现有记忆'
export const queueHoldLabels: Record<QueueHoldReason, string> = {
  active_turn: '等待当前回合结束', steering_settlement: '等待补充消息结清', provider_serial: '等待 AI 空闲',
  task_limit: '等待 Bot 任务名额', machine_slot: '等待设备运行名额', pending_check: '等待确认上一轮结果',
  backend_config_pending: '等待当前 AI 回合结束后切换配置',
}
export const agentLaneLabels: Record<AgentLane, string> = { main: '主对话', task: '独立任务', schedule_routine: '定时任务', bot_message: 'Bot 消息' }

const stateNames: Record<string, string> = {
  accepted: "已接收",
  pending_delivery: "等待设备接收",
  queued: "等待处理",
  steering: "Bot 正在看这条补充",
  steered: "已补充给 Bot",
  claimed: "正在准备",
  running: "执行中",
  waiting: "需要你同意",
  cancelling: "正在确认是否已取消",
  interrupting: "正在确认是否已停止",
  interrupted: "已中断",
  starting: "正在启动",
  unverified: "待确认",
  stopped: "已停止",
  unknown: "结果未知",
  pending_check: "待确认",
  succeeded: "操作完成",
  failed: "失败",
  rejected: "已拒绝",
  open: "已打开",
  opening: "正在打开",
  working: "处理中",
  needs_review: "待验收",
  idle: "空闲",
  pending: "待处理",
  completed: "已完成",
  awaiting_acceptance: "待验收",
  cancelled: "已取消",
  blocked: "受阻",
  ready: "就绪",
  awaiting_input: "待处理",
  finishing: "收尾中",
}

type NodeNameLookup = {
  nodes?: readonly NodeInfo[]
  directory?: MembershipView | null
}

export function nodeName(
  id: string | undefined,
  lookup?: NodeNameLookup,
  fallback?: string,
) {
  if (!id) return "未知设备"
  const node = lookup?.nodes?.find((item) => item.device_id === id)
  if (node?.name) return node.name
  const member = lookup?.directory?.members.find((item) => item.device_id === id)
  if (member?.name) return member.name
  if (node?.local) return "本机"
  return fallback ?? "另一台设备"
}

export function isLoopback(address: string) {
  try { return ["localhost", "127.0.0.1", "[::1]"].includes(new URL(address).hostname) } catch { return false }
}

/** Short display name for an access address. */
export function addressName(address: string) {
  if (isLoopback(address)) return "这台电脑"
  try { return new URL(address).hostname } catch { return address }
}

/**
 * The connected computer's name for the connection capsule. A member reads it from the directory; until the directory
 * lists that computer it is `null`, since the computer's own name may predate its join. Before joining, its own name.
 */
export function entryLabel(input: { joined: boolean; directory: Pick<MembershipView, "members"> | null | undefined; entryId: string | undefined; ownName: string | undefined; address: string }): string | null {
  const member = input.directory?.members.find((item) => item.device_id === input.entryId)?.name
  if (member) return member
  return input.joined ? null : input.ownName || addressName(input.address)
}

/** Joins and members carry only node or client, so a client named like a tablet or a phone counts as one, everywhere it shows. */
export type DeviceForm = "computer" | "tablet" | "phone" | "browser"
export const deviceForm = (kind: "node" | "client", name: string): DeviceForm =>
  kind === "node" ? "computer" : /ipad|tablet|平板/i.test(name) ? "tablet" : /iphone|android|phone|手机/i.test(name) ? "phone" : "browser"
export const formName: Record<DeviceForm, string> = { computer: "电脑", tablet: "平板", phone: "手机", browser: "浏览器" }

/**
 * A computer's reachability as the Node reports it; `entry` marks the computer this device connects through, online
 * before that read. Another browser or phone has none: only the Node it happens to be connected to would know; `self`,
 * the device in hand, is online while its connection is. While this device is offline the last report is stale: the
 * computer it lost reads offline, the others unknown.
 */
export const deviceOnline = (kind: "node" | "client", node: Pick<NodeInfo, "online"> | undefined, entry: boolean, connected: boolean, self = false) =>
  kind !== "node" ? (self ? connected : undefined) : !connected ? (entry ? false : undefined) : node?.online ?? entry
export const hostOf = (address: string) => { try { return new URL(address).host } catch { return "" } }

const taskStates: Record<string, string> = {
  pending_delivery: "等待中", queued: "等待中", claimed: "准备中", accepted: "准备中", running: "运行中", waiting: "需要你同意",
  cancelling: "停止中", interrupting: "停止中", completed: "已完成", failed: "失败", cancelled: "已停止", interrupted: "已中断",
  pending_check: "待确认", unknown: "待确认", starting: "准备中", finished: "已完成", unverified: "待确认", awaiting_confirmation: "待确认",
  dispatch_waiting: "等设备接收", dispatch_rejected: "未接收", awaiting_children: "等子任务", dispatch_unverified: "结果未知", accepted_waiting_sync: "等待同步", stop_waiting: "停止中", stop_timed_out: "还在尝试停止",
}
export const taskLabel = (state: string) => taskStates[state] ?? stateNames[state] ?? state
/** A task still running when this device went offline may have moved on since; it reads unknown, not running. */
export const taskStale = (state: string, connected: boolean) => !connected && taskActive(state)
export const staleTaskLabel = "状态未知"
/** A task that stopped without a cause of its own: stopped by hand, or its device or app went away mid-turn. Nothing redoes it. */
export const interruptedCopy = { reason: "中途停下了，不会自动重做" }
/** A task sent to a device that is offline starts once that device is back. */
export const taskCopy = { away: "离线，上线后开始" }
/** What the person asks the bot after a task ended badly, in both apps. */
export type FollowUp = "check" | "resume" | "retry"
export const followUpCopy: Record<FollowUp, { action: string; ask: (title: string) => string }> = {
  check: { action: "让 Bot 查结果", ask: (title) => `看看「${title}」做完了没有，先不要重做` },
  resume: { action: "让 Bot 接着做", ask: (title) => `接着做「${title}」` },
  retry: { action: "让 Bot 再试", ask: (title) => `再试一次「${title}」` },
}
/** An unknown outcome may have done its writes, so the bot checks it rather than running it again; a stopped task goes on. */
export const followUp = (state: string, unknown: boolean): FollowUp => unknown ? "check" : state === "interrupted" ? "resume" : "retry"

// Live progress reports provider words such as "running" or "awaiting_input"; anything unknown reads as plain progress.
const progressStates: Record<string, string> = {
  running: "进行中", in_progress: "进行中", finishing: "收尾中", completed: "已完成", failed: "失败", interrupted: "已中断",
  unknown: "结果未知", awaiting_input: "待处理", declined: "已拒绝",
}
export const progressLabel = (status: string) => progressStates[status] ?? (/[\u4e00-\u9fff]/.test(status) ? status : "进行中")

/**
 * Who runs a task, the same on every card: a provider names itself; an agent task runs as its device's bot turn, so
 * it names the provider that bot runs on there.
 */
export const taskAgent = (work: Pick<WorkRecord, "kind" | "target_node">, nodes: readonly NodeInfo[]): ProviderId | null =>
  work.kind === "task" ? asProvider(nodes.find((node) => node.device_id === work.target_node)?.capabilities?.manager) : work.kind ?? null
/** A task's newest step: what it recorded, else the last line or status its device streams, as another device's bot records none. */
export function taskProgress(work: Pick<WorkRecord, "work_id" | "target_node" | "summary">, live: readonly LiveProgress[]) {
  if (work.summary) return work.summary
  const stream = live.find((item) => item.kind === "native" && item.work_id === work.work_id && item.target_node === work.target_node)
  const line = stream?.text.trim().split("\n").at(-1) ?? ""
  if (line) return line
  const status = stream?.status ? progressLabel(stream.status) : ""
  return status === "进行中" ? "" : status
}

/**
 * A work only the person can settle now reads as awaiting confirmation; a finished one as its outcome; a stop not yet
 * confirmed as that stop, which keeps being retried.
 */
export const shownState = (work: WorkRecord, nodes: NodeInfo[] = []) =>
  confirmReason(work, nodes) ? "awaiting_confirmation" : work.outcome ?? work.stop ?? (work.state === "finished" ? "unknown" : work.state)
export const taskActive = (state: string) => ["accepted", "running", "interrupting", "dispatch_waiting", "stop_waiting", "stop_timed_out"].includes(state)
/**
 * A chat task's state: one that completed while a work it dispatched, or that work's own, still runs waits on them. No record
 * says so, so this reads only the works this device holds; a child not synced yet does not hold its parent.
 */
export function chatTaskState(work: WorkRecord, works: readonly WorkRecord[], nodes: NodeInfo[] = []): string {
  const state = shownState(work, nodes)
  if (state !== "completed") return state
  const seen = new Set([work.work_id])
  const open = (id: string): boolean => works.some((child) => {
    if (child.parent !== id || seen.has(child.work_id)) return false
    seen.add(child.work_id)
    const shown = shownState(child, nodes)
    return taskActive(shown) || shown === "awaiting_confirmation" || open(child.work_id)
  })
  return open(work.work_id) ? "awaiting_children" : state
}

/** A failed work, or a dispatch its device refused: the person sees it until something follows it up. */
export const failedWork = (work: Pick<WorkRecord, "outcome" | "state">) => work.outcome === "failed" || work.outcome === "rejected" || work.state === "dispatch_rejected"
/** A failure fixed in its device's settings: no bot there to take it, or the AI service there not set up. */
export const fixOnDevice = (work: Pick<WorkRecord, "failure">) =>
  ["missing_key", "auth_failed", "missing_binary", "execution_disabled", "no_model", "no_bot"].includes(work.failure ?? "")
/** A prompt whose whole text or first line parses as JSON is machine text; plain words that merely open with "[" or "{" are not. */
function machineText(text: string) {
  if (!/^[{[]/.test(text)) return false
  const parses = (value: string) => { try { JSON.parse(value); return true } catch { return false } }
  return parses(text) || parses(text.split(/\r?\n/, 1)[0])
}
/** Characters as a person sees them, so a cut never splits an emoji; Hermes without `Intl.Segmenter` falls back to code points. */
function graphemes(text: string): string[] {
  return typeof Intl !== "undefined" && typeof Intl.Segmenter === "function" ? Array.from(new Intl.Segmenter(undefined, { granularity: "grapheme" }).segment(text), (part) => part.segment) : Array.from(text)
}
/**
 * What a task was asked, short enough for its page: the first plain line, cut at 80 characters. Machine text such as JSON gives none,
 * and neither does a line that only repeats the title. `folded`: the prompt says more than the summary, so its text waits in 详细信息.
 */
export function promptSummary(prompt: string, title: string): { summary: string; folded: boolean } {
  const text = prompt.trim()
  const first = machineText(text) ? "" : text.split(/\r?\n/).map((line) => line.replace(/^#+\s*/, "").trim()).find(Boolean) ?? ""
  const chars = graphemes(first)
  const cut = chars.length > 80 ? `${chars.slice(0, 79).join("")}…` : first
  const summary = cut === title.trim() ? "" : cut
  return { summary, folded: summary !== text && text !== title.trim() }
}
export const fixOnDeviceLabel = (device: string) => `去设置「${device}」`

/** Where a task card sits on the tasks page, in both apps. */
export type TaskGroup = "attention" | "working" | "waiting"
export const taskGroups: readonly { group: TaskGroup; title: string }[] = [{ group: "attention", title: "需要留意" }, { group: "working", title: "进行中" }, { group: "waiting", title: "等待中" }]
/** DAG results survive an older running read; unknown or conflicting results have no ordering proof. */
export function cardState(work: WorkRecord, nodes: NodeInfo[], projected?: Pick<OverviewTask, "state" | "outcome">): string {
  if (work.state === "unverified" || work.outcome === "unverified" || work.state === "dispatch_unverified") return shownState(work, nodes)
  if (projected && (projected.outcome === "unverified" || ["unknown", "unverified", "dispatch_unverified"].includes(projected.state))) return projected.outcome === "unverified" ? "unverified" : projected.state
  if ((work.state === "finished" && !work.outcome) || (projected?.state === "finished" && !projected.outcome)) return "unknown"
  if (work.outcome && projected?.outcome && work.outcome !== projected.outcome) return "unknown"
  const outcome = work.outcome ?? projected?.outcome
  if (outcome) return outcome
  return confirmReason(work, nodes) ? "awaiting_confirmation" : work.stop ?? projected?.state ?? work.state
}

/** An entry task check only folds into its unverified work; a bounded Bot list does not prove deletion. */
export function unrepresentedOverviewChecks(overview: WorkspaceOverview, works: readonly OpenWork[], nodes: NodeInfo[], knownBots: readonly OverviewBot[] = []) {
  const local = nodes.find((node) => node.local)?.device_id
  return overview.tasks.filter((check): check is Extract<OverviewTask, { kind: "check" }> => {
    if (check.kind !== "check" || check.target_node !== null) return false
    if (!check.work_id || !local) return true
    return !works.some((work) => {
      if (work.conversation_id !== check.conversation_id || work.work_id !== check.work_id || work.target_node !== local) return false
      const projected = overview.tasks.find((item) => item.kind !== "check" && item.conversation_id === work.conversation_id && item.work_id === work.work_id)
      if (work.state !== "unverified" && projected?.state !== "unverified") return false
      return ["awaiting_confirmation", "unverified"].includes(cardState(work, nodes, projected))
    })
  }).map((check) => {
    if (check.bot) return check
    const bot = knownBots.find((bot) => bot.conversation_id === check.conversation_id)
    return bot ? { ...check, bot, navigation: { bot_id: bot.bot_id, conversation_id: check.conversation_id } } : check
  })
}

type WorkspaceActivityInput = {
  works: readonly OpenWork[]
  questions: readonly Question[]
  checks: readonly PendingCheck[]
  overview?: WorkspaceOverview | null
  jobs?: readonly WorkspaceActivityJob[]
  answered?: Readonly<Record<string, string>>
  nodes?: readonly NodeInfo[]
  knownBots?: readonly OverviewBot[]
  connected?: boolean
  read?: boolean
}

const activityKey = (conversationId: string, workId: string) => JSON.stringify([conversationId, workId])
const emptyActivityFacts = (): WorkspaceActivityFacts => ({ questions: [], checks: [], overviewQuestions: [], overviewChecks: [], jobs: [] })
const activityJobId = (conversationId: string | null, job: ManagerJob | OverviewJob) => `turn:${conversationId ?? ""}:${job.trigger}`
const taskWorkId = (trigger: string) => trigger.startsWith("task:") ? trigger.slice(5) || null : null

/** Build the shared Work/Turn/Attention activity domain from already loaded facts. */
export function workspaceActivities(input: WorkspaceActivityInput): WorkspaceActivityView {
  const nodes = input.nodes ?? []
  const localNode = nodes.find((node) => node.local)?.device_id
  const answered = input.answered ?? {}
  const overview = input.overview ?? null
  const knownBots = input.knownBots ?? []
  const botCatalog = input.read === false ? knownBots : [...knownBots, ...(overview?.bots ?? [])]
  const enrichCheck = (task: Extract<OverviewTask, { kind: "check" }>) => {
    if (task.bot) return task
    const bot = botCatalog.find((item) => item.conversation_id === task.conversation_id)
    return bot ? { ...task, bot, navigation: { bot_id: bot.bot_id, conversation_id: task.conversation_id } } : task
  }
  const projectionByKey = new Map<string, Extract<OverviewTask, { kind: "work" | "dispatch" }>>()
  for (const task of overview?.tasks ?? []) if ((task.kind === "work" || task.kind === "dispatch") && task.work_id) projectionByKey.set(activityKey(task.conversation_id, task.work_id), task)
  const workByKey = new Map<string, OpenWork>()
  for (const work of input.works) workByKey.set(activityKey(work.conversation_id, work.work_id), work)
  const liveQuestionIds = new Set(input.questions.map((question) => question.question_id))
  const attachedQuestions = new Set<string>()
  const attachedOverviewChecks = new Set<string>()
  const attachedJobs = new Set<string>()
  const activities: WorkspaceActivity[] = []
  const workActivities = new Map<string, WorkspaceActivity>()
  const keyForCheck = (check: PendingCheck) => check.kind === "work" ? activityKey(check.conversation_id, check.work_id) : null
  const checkMatchesWork = (check: PendingCheck, work: OpenWork) => check.kind === "work"
    ? check.conversation_id === work.conversation_id && check.work_id === work.work_id && check.target_node === work.target_node
    : check.kind === "device" && check.works.some((item) => item.conversation_id === work.conversation_id && item.work_id === work.work_id && item.target_node === work.target_node)
  for (const work of input.works) {
    const key = activityKey(work.conversation_id, work.work_id)
    if (workActivities.has(key)) continue
    const projection = projectionByKey.get(key)
    const state = cardState(work, [...nodes], projection)
    const effectiveState = taskStale(state, Boolean(input.connected && input.read)) ? "unknown" : state
    const facts = emptyActivityFacts()
    for (const question of input.questions) {
      if (attachedQuestions.has(question.question_id)) continue
      if (!awaitsAnswer(question, answered) || workApproval({ work, conversationId: work.conversation_id, questions: [question], answered })?.question_id !== question.question_id) continue
      facts.questions.push(question); attachedQuestions.add(question.question_id)
    }
    for (const check of input.checks) {
      if (!checkMatchesWork(check, work)) continue
      facts.checks.push(check)
    }
    for (const task of overview?.tasks ?? []) {
      if (task.kind !== "check" || task.conversation_id !== work.conversation_id || task.work_id !== work.work_id) continue
      if (localNode && work.target_node !== localNode) continue
      if (task.target_node !== null && task.target_node !== work.target_node) continue
      if (task.target_node !== null && localNode && task.target_node !== localNode) continue
      if (localNode && !["awaiting_confirmation", "unknown", "unverified"].includes(cardState(work, [...nodes], projection))) continue
      facts.overviewChecks.push(enrichCheck(task)); attachedOverviewChecks.add(task.check_id)
    }
    const activity: WorkspaceActivity = { id: `work:${work.conversation_id}:${work.work_id}`, kind: "work", conversation_id: work.conversation_id, work_id: work.work_id, group: taskGroup(effectiveState, facts.questions.length > 0 || facts.checks.length > 0), state: effectiveState, work, projection, facts }
    workActivities.set(key, activity); activities.push(activity)
  }
  const jobs = [
    ...(input.jobs ?? []),
    ...(overview?.jobs ?? []).map((job) => ({ conversation_id: job.conversation_id, job })),
  ]
  for (const entry of jobs) {
    const workId = taskWorkId(entry.job.trigger)
    const key = workId && entry.conversation_id ? activityKey(entry.conversation_id, workId) : null
    const workActivity = key ? workActivities.get(key) : undefined
    if (workActivity) { workActivity.facts.jobs.push(entry.job); attachedJobs.add(activityJobId(entry.conversation_id, entry.job)); continue }
    const id = activityJobId(entry.conversation_id, entry.job)
    if (attachedJobs.has(id)) continue
    const state = entry.job.state === "unknown" ? "unknown" : entry.job.state
    const history = schedulerStatus(state).group === "history"
    if (history && !["failed", "rejected"].includes(state) && !entry.job.pending_check) { attachedJobs.add(id); continue }
    const group: WorkspaceActivityGroup = ["unknown", "unverified", "failed", "rejected"].includes(state) ? "attention" : taskActive(state) ? "working" : "waiting"
    activities.push({ id, kind: "turn", conversation_id: entry.conversation_id, work_id: null, group, state, job: entry.job, facts: emptyActivityFacts() }); attachedJobs.add(id)
  }
  const addAttention = (id: string, conversationId: string | null, attention: WorkspaceActivityAttention, state = "attention") => {
    activities.push({ id, kind: "attention", conversation_id: conversationId, work_id: null, group: "attention", state, attention, facts: emptyActivityFacts() })
  }
  for (const question of input.questions) if (awaitsAnswer(question, answered) && !attachedQuestions.has(question.question_id)) addAttention(`attention:question:${question.question_id}`, question.conversation_id, question)
  for (const check of input.checks) {
    const checkKey = keyForCheck(check)
    const attached = check.kind === "device"
      ? check.works.some((item) => workActivities.has(activityKey(item.conversation_id, item.work_id)) && workByKey.get(activityKey(item.conversation_id, item.work_id)) && checkMatchesWork(check, workByKey.get(activityKey(item.conversation_id, item.work_id))!))
      : checkKey ? workByKey.get(checkKey) ? checkMatchesWork(check, workByKey.get(checkKey)!) : false : false
    if (!attached) addAttention(`attention:check:${check.check_id}`, check.kind === "work" ? check.conversation_id : null, check)
  }
  for (const question of overview?.questions ?? []) if (!(question.question_id in answered) && !liveQuestionIds.has(question.question_id)) addAttention(`attention:overview-question:${question.question_id}`, question.conversation_id, question)
  for (const task of overview?.tasks ?? []) if (task.kind === "check" && !attachedOverviewChecks.has(task.check_id)) addAttention(`attention:overview-check:${task.item_id}`, task.conversation_id, enrichCheck(task))
  const groups: Record<WorkspaceActivityGroup, WorkspaceActivity[]> = { attention: [], working: [], waiting: [] }
  for (const activity of activities) groups[activity.group].push(activity)
  return { activities, groups }
}
export const needsLook = (state: string) => ["failed", "rejected", "dispatch_rejected", "dispatch_unverified"].includes(state)
/** A task that failed, was never taken or awaits the person's confirmation needs them; one out of their hands or held on them waits; the rest work. */
export const taskGroup = (state: string, heldOnPerson: boolean): TaskGroup =>
  needsLook(state) || ["awaiting_confirmation", "unverified", "unknown"].includes(state) ? "attention" : heldOnPerson || !taskActive(state) ? "waiting" : "working"

export type SummaryStep = { state: "done" | "current" | "pending"; text: string }

/** Only a summary made entirely of marked lines is a step list. It is the latest summary, not execution history. */
export function parseSummarySteps(summary: string): SummaryStep[] | null {
  if (!summary) return null
  const steps: SummaryStep[] = []
  for (const line of summary.split(/\r?\n/)) {
    const match = /^([✓→·]) +(.+)$/.exec(line)
    if (!match || !match[2].trim()) return null
    steps.push({ state: match[1] === "✓" ? "done" : match[1] === "→" ? "current" : "pending", text: match[2] })
  }
  return steps
}

const schedulerStates: Record<string, { label: string; group: "open" | "history" }> = {
  queued: { label: "排队中", group: "open" }, claimed: { label: "已领取", group: "open" }, running: { label: "执行中", group: "open" },
  steering: { label: "补充中", group: "open" }, cancelling: { label: "停止中", group: "open" }, publishing: { label: "回复同步中", group: "open" },
  pending_delivery: { label: "等待设备接收", group: "open" }, steered: { label: "已补充", group: "history" }, completed: { label: "已完成", group: "history" },
  failed: { label: "失败", group: "history" }, cancelled: { label: "已取消", group: "history" }, interrupted: { label: "已中断", group: "history" },
}
export const schedulerStatus = (state: string): { label: string; group: "open" | "history" | "unknown" } => schedulerStates[state] ?? { label: "状态未知", group: "unknown" }

/** Muting is independent of the device's reachability and never proves that a provider session is alive. */
export function externalSessionStatus(session: Pick<SessionHandoff, "node">, nodes: readonly NodeInfo[]): "revoked" | "unknown" | "offline" | "reachable" {
  const node = nodes.find((item) => item.device_id === session.node)
  return node?.revoked ? "revoked" : !node ? "unknown" : !node.online ? "offline" : "reachable"
}
export const externalSessionLabels = { revoked: "设备已撤权", unknown: "设备可达性未知", offline: "设备不可达", reachable: "设备可达" }

export function overviewScope(coverage: OverviewCoverage): string[] {
  const notes = [coverage.source === "workspace_overview" ? "当前已同步到接入设备的记录" : "当前已加载的本地记录"]
  if (!coverage.complete || coverage.consistency === "partial") notes.push("部分记录，不代表全部任务")
  if (coverage.truncated) notes.push("总览已截断，保留其他已知任务")
  const omitted = [["任务", coverage.omitted_tasks], ["问题", coverage.omitted_questions], ["检查", coverage.omitted_checks], ["派发", coverage.omitted_dispatches], ["发送记录", coverage.omitted_deliveries], ["后台工作", coverage.omitted_jobs]] as const
  for (const [label, count] of omitted) if (count !== null && count > 0) notes.push(`本次总览未返回 ${count} 项${label}`)
  if (coverage.remote_owners.some((owner) => owner.reason === "remote_scheduler_not_queried" || owner.status === "remote_scheduler_not_queried")) notes.push("尚未读取其他设备的后台工作")
  if (coverage.unreachable_nodes.length) notes.push("部分设备不可达，状态可能滞后")
  return notes
}
