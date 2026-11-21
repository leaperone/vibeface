import { awaitsAnswer, workApproval } from "./questions.ts"
import { cardState, schedulerStatus, taskActive, taskGroup, taskStale } from "./labels.ts"
import type { ManagerJob, NodeInfo, OpenWorkCard, OverviewBot, OverviewJob, OverviewTask, PendingCheck, Question, WorkspaceActivity, WorkspaceActivityAttention, WorkspaceActivityFacts, WorkspaceActivityGroup, WorkspaceActivityJob, WorkspaceActivityView, WorkspaceOverview } from "./types.ts"
import { isJobState, isTerminalJobState, jobStates } from "./types.ts"

export type WorkspaceActivityInput = {
  works: readonly OpenWorkCard[]
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
const questionKey = (conversationId: string, questionId: string) => JSON.stringify([conversationId, questionId])
const emptyActivityFacts = (): WorkspaceActivityFacts => ({ questions: [], checks: [], overviewQuestions: [], overviewChecks: [], jobs: [] })
const activityJobId = (conversationId: string | null, job: ManagerJob | OverviewJob) => `turn:${conversationId ?? ""}:${"source_node" in job ? job.source_node ?? "unknown" : "local"}:${"job_id" in job ? job.job_id : job.trigger}`
const taskWorkId = (trigger: string) => trigger.startsWith("task:") ? trigger.slice(5) || null : null

const jobSource = (entry: WorkspaceActivityJob, localNode: string | undefined) => {
  if ("job_id" in entry.job) return entry.job.source_node
  return entry.job.source_node ?? localNode ?? null
}
const sameObservedJob = (left: WorkspaceActivityJob, right: WorkspaceActivityJob, localNode: string | undefined) => {
  if (!left.conversation_id || left.conversation_id !== right.conversation_id || !left.job.trigger || left.job.trigger !== right.job.trigger) return false
  const leftSource = jobSource(left, localNode), rightSource = jobSource(right, localNode)
  return leftSource !== null && leftSource === rightSource
}
const jobFailure = (job: ManagerJob | OverviewJob) => "failure" in job ? job.failure : undefined

function mergeJobObservations(entries: readonly WorkspaceActivityJob[]): WorkspaceActivityJob {
  const first = entries[0]
  if (!first) throw new Error("任务观察不能为空。")
  const jobs = entries.map((entry) => entry.job)
  const states = new Set(jobs.map((job) => job.state))
  const terminalStates = new Set(jobs.map((job) => job.state).filter(isTerminalJobState))
  const outcomes = new Set(jobs.flatMap((job) => job.outcome ? [job.outcome] : []))
  const runtimes = jobs.flatMap((job) => job.runtime ? [job.runtime] : [])
  const runtimeIds = new Set(runtimes.map((runtime) => JSON.stringify([runtime.delivery_id, runtime.generation, runtime.session_generation, runtime.provider, runtime.model, runtime.reasoning_effort])))
  const ownerEpochs = new Set(jobs.flatMap((job) => "owner_epoch" in job && job.owner_epoch !== null ? [job.owner_epoch] : []))
  const hasUnknownState = jobs.some((job) => !isJobState(job.state))
  const pendingCheck = jobs.some((job) => job.pending_check === true)
  const queueStates = new Set(["queued", "pending_delivery"])
  const recoveryStates = new Set(["claimed", "steering"])
  const recoveryRollback = jobs.some((job) => queueStates.has(job.state)) && jobs.some((job) => recoveryStates.has(job.state))
  const attemptConflict = runtimeIds.size > 1 || ownerEpochs.size > 1
  const uncertain = pendingCheck || hasUnknownState || states.has("unknown") || states.has("unverified") || terminalStates.size > 1 || outcomes.size > 1 || (terminalStates.size > 0 && jobs.some((job) => isJobState(job.state) && jobStates[job.state].phase === 'active')) || recoveryRollback || attemptConflict || jobs.some((job) => job.state === "steered" && job.outcome === "unknown")
  const state = uncertain ? "unknown" : [...states].filter(isJobState).sort((left, right) => jobStates[right].rank - jobStates[left].rank)[0] ?? "unknown"
  const primary = entries.find((entry) => entry.job.state === state) ?? entries[0]
  const failure = jobs.map(jobFailure).find(Boolean)
  const runtime = primary.job.runtime ?? runtimes[0]
  const retryable = state === "failed" && !pendingCheck && runtime?.execution === "not_started" && jobs.some((job) => job.retryable === true)
  return {
    conversation_id: primary.conversation_id,
    job: {
      ...primary.job,
      state,
      ...(runtime ? { runtime } : {}),
      ...(failure ? { failure } : {}),
      retryable,
      ...(uncertain ? { outcome: "unknown" as const } : {}),
      ...(pendingCheck ? { pending_check: true } : {}),
    },
  }
}

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
  const workByKey = new Map<string, OpenWorkCard>()
  for (const work of input.works) workByKey.set(activityKey(work.conversation_id, work.work_id), work)
  const liveQuestionIds = new Set(input.questions.map((question) => questionKey(question.conversation_id, question.question_id)))
  const attachedQuestions = new Set<string>()
  const attachedOverviewChecks = new Set<string>()
  const attachedJobs = new Set<string>()
  const activities: WorkspaceActivity[] = []
  const workActivities = new Map<string, WorkspaceActivity>()
  const keyForCheck = (check: PendingCheck) => check.kind === "work" ? activityKey(check.conversation_id, check.work_id) : null
  const checkMatchesWork = (check: PendingCheck, work: OpenWorkCard) => check.kind === "work"
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
      if (attachedQuestions.has(questionKey(question.conversation_id, question.question_id))) continue
      if (!awaitsAnswer(question, answered) || workApproval({ work, conversationId: work.conversation_id, questions: [question], answered })?.question_id !== question.question_id) continue
      facts.questions.push(question); attachedQuestions.add(questionKey(question.conversation_id, question.question_id))
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
  const rawJobs: WorkspaceActivityJob[] = [
    ...(input.jobs ?? []),
    ...(overview?.jobs ?? []).map((job) => ({ conversation_id: job.conversation_id, job })),
  ]
  const jobGroups: WorkspaceActivityJob[][] = []
  for (const entry of rawJobs) {
    const group = jobGroups.find((items) => sameObservedJob(items[0]!, entry, localNode))
    if (group) group.push(entry)
    else jobGroups.push([entry])
  }
  for (const observations of jobGroups) {
    const entry = mergeJobObservations(observations)
    const workId = taskWorkId(entry.job.trigger)
    const key = workId && entry.conversation_id ? activityKey(entry.conversation_id, workId) : null
    const workActivity = key ? workActivities.get(key) : undefined
    if (workActivity) { workActivity.facts.jobs.push(...observations.map((item) => item.job)); attachedJobs.add(activityJobId(entry.conversation_id, entry.job)); continue }
    const id = activityJobId(entry.conversation_id, entry.job)
    if (attachedJobs.has(id)) continue
    const state = entry.job.state === "unknown" ? "unknown" : entry.job.state
    const history = schedulerStatus(state).group === "history"
    if (history && !["failed", "rejected"].includes(state) && !entry.job.pending_check) { attachedJobs.add(id); continue }
    const activityGroup: WorkspaceActivityGroup = ["unknown", "unverified", "failed", "rejected"].includes(state) ? "attention" : taskActive(state) ? "working" : "waiting"
    activities.push({ id, kind: "turn", conversation_id: entry.conversation_id, work_id: null, group: activityGroup, state, job: entry.job, facts: { ...emptyActivityFacts(), jobs: observations.map((item) => item.job) } }); attachedJobs.add(id)
  }
  const addAttention = (id: string, conversationId: string | null, attention: WorkspaceActivityAttention, state = "attention") => {
    activities.push({ id, kind: "attention", conversation_id: conversationId, work_id: null, group: "attention", state, attention, facts: emptyActivityFacts() })
  }
  for (const question of input.questions) if (awaitsAnswer(question, answered) && !attachedQuestions.has(questionKey(question.conversation_id, question.question_id))) addAttention(`attention:question:${question.question_id}`, question.conversation_id, question)
  for (const check of input.checks) {
    const checkKey = keyForCheck(check)
    const attached = check.kind === "device"
      ? check.works.some((item) => workActivities.has(activityKey(item.conversation_id, item.work_id)) && workByKey.get(activityKey(item.conversation_id, item.work_id)) && checkMatchesWork(check, workByKey.get(activityKey(item.conversation_id, item.work_id))!))
      : checkKey ? workByKey.get(checkKey) ? checkMatchesWork(check, workByKey.get(checkKey)!) : false : false
    if (!attached) addAttention(`attention:check:${check.check_id}`, check.kind === "work" ? check.conversation_id : null, check)
  }
  for (const question of overview?.questions ?? []) if (!(question.question_id in answered) && !liveQuestionIds.has(questionKey(question.conversation_id, question.question_id))) addAttention(`attention:overview-question:${question.question_id}`, question.conversation_id, question)
  for (const task of overview?.tasks ?? []) if (task.kind === "check" && !attachedOverviewChecks.has(task.check_id)) addAttention(`attention:overview-check:${task.item_id}`, task.conversation_id, enrichCheck(task))
  const groups: Record<WorkspaceActivityGroup, WorkspaceActivity[]> = { attention: [], working: [], waiting: [] }
  for (const activity of activities) groups[activity.group].push(activity)
  return { activities, groups }
}
