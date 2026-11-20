import { cardState, workspaceActivities } from "./labels.ts"
import { openWorksFromOverview, overviewFromWorkspace } from "./model.ts"
import { questionOpen } from "./questions.ts"
import type {
  BotRecord,
  NodeInfo,
  OpenWorkCard,
  OverviewCoverage,
  PendingCheck,
  Question,
  WorkspaceActivityJob,
  WorkspaceActivityView,
  WorkspaceAssignment,
  WorkspaceDomainState,
  WorkspaceOverview,
  WorkspaceTask,
} from "./types.ts"

/** The two existing platform policies plus an explicit shared policy for fixture comparison. */
export type TerminalCheckPolicy = "web-filtered" | "mobile-inclusive" | "shared"

export type OverviewInputPolicy = {
  /** Local open work facts. These remain the fallback when no remote overview exists. */
  openWorks: readonly OpenWorkCard[]
  bots: readonly BotRecord[]
  questions: readonly Question[]
  nodes: readonly NodeInfo[]
  checks: readonly PendingCheck[]
  jobs?: readonly WorkspaceActivityJob[]
  /** `overview` is the name used by current callers; `remoteOverview` is an explicit alias. */
  overview?: WorkspaceOverview | null
  remoteOverview?: WorkspaceOverview | null
  answered?: Readonly<Record<string, string>>
  read?: boolean
  connected?: boolean
  terminalCheckPolicy: TerminalCheckPolicy
  botFilter?: string
  /** Work IDs already represented by a workspace assignment. */
  assignedWorkIds?: readonly string[]
  assignmentWorkIds?: readonly string[]
  assignments?: readonly Pick<WorkspaceAssignment, "work_id">[]
  workspaceState?: Pick<WorkspaceDomainState, "tasks" | "assignments">
  tasks?: readonly Pick<WorkspaceTask, "task_id">[]
}

export type ExcludedTerminalCheck = {
  check_id: string
  work_id: string
  reason: "terminal-work"
  policy: "web-filtered"
}

export type PreparedOverviewInputs = {
  /** The normalized work set used for rows and activity projection. */
  works: OpenWorkCard[]
  questions: Question[]
  checks: PendingCheck[]
  jobs: WorkspaceActivityJob[]
  overview: WorkspaceOverview
  coverage: OverviewCoverage
  activities: WorkspaceActivityView
  terminalCheckPolicy: TerminalCheckPolicy
  excludedTerminalChecks: ExcludedTerminalCheck[]
  /** Alias useful to adapters that describe these as filtering reasons. */
  terminalCheckReasons: ExcludedTerminalCheck[]
}

const checkMatchesWork = (check: PendingCheck, work: OpenWorkCard) => check.kind === "work"
  ? check.conversation_id === work.conversation_id && check.work_id === work.work_id && check.target_node === work.target_node
  : check.kind === "device" && check.works.some((item) => item.conversation_id === work.conversation_id && item.work_id === work.work_id && item.target_node === work.target_node)

const terminalStates = new Set(["completed", "failed", "interrupted", "rejected"])

function assignedIds(input: OverviewInputPolicy): ReadonlySet<string> {
  const values = [
    ...(input.assignedWorkIds ?? []),
    ...(input.assignmentWorkIds ?? []),
    ...(input.assignments ?? []).flatMap((assignment) => assignment.work_id ? [assignment.work_id] : []),
    ...(input.workspaceState?.assignments ?? []).flatMap((assignment) => assignment.work_id ? [assignment.work_id] : []),
  ]
  return new Set(values)
}

function scopeOverview(overview: WorkspaceOverview, input: OverviewInputPolicy, assigned: ReadonlySet<string>): WorkspaceOverview {
  if ((!input.botFilter || input.botFilter === "all") && assigned.size === 0) return overview
  const taskVisible = (task: WorkspaceOverview["tasks"][number]) => !task.work_id || !assigned.has(task.work_id) && botMatches(input.botFilter, task.conversation_id, input.bots)
  return {
    ...overview,
    tasks: overview.tasks.filter(taskVisible),
    questions: overview.questions.filter((question) => botMatches(input.botFilter, question.conversation_id, input.bots)),
    jobs: overview.jobs.filter((job) => !job.conversation_id || botMatches(input.botFilter, job.conversation_id, input.bots)),
  }
}

function botMatches(botFilter: string | undefined, conversationId: string, bots: readonly BotRecord[]): boolean {
  if (!botFilter || botFilter === "all") return true
  return bots.some((bot) => bot.bot_id === botFilter && bot.conversation_id === conversationId)
}

function filterWorks(works: readonly OpenWorkCard[], input: OverviewInputPolicy, assigned: ReadonlySet<string>): OpenWorkCard[] {
  return works.filter((work) => !assigned.has(work.work_id) && botMatches(input.botFilter, work.conversation_id, input.bots))
}

function filterQuestions(questions: readonly Question[], input: OverviewInputPolicy): Question[] {
  return questions.filter((question) => questionOpen(question) && botMatches(input.botFilter, question.conversation_id, input.bots))
}

function filterChecks(
  checks: readonly PendingCheck[],
  works: readonly OpenWorkCard[],
  overview: WorkspaceOverview,
  input: OverviewInputPolicy,
): { checks: PendingCheck[]; excluded: ExcludedTerminalCheck[] } {
  const excluded: ExcludedTerminalCheck[] = []
  if (input.terminalCheckPolicy !== "web-filtered") return { checks: [...checks], excluded }
  const projectionByKey = new Map(overview.tasks.flatMap((task) => {
    if ((task.kind !== "work" && task.kind !== "dispatch") || !task.work_id) return []
    return [[JSON.stringify([task.conversation_id, task.work_id]), task] as const]
  }))
  const kept = checks.filter((check) => {
    if (check.kind !== "work") return true
    const work = works.find((candidate) => checkMatchesWork(check, candidate))
    if (!work) return true
    const projection = projectionByKey.get(JSON.stringify([work.conversation_id, work.work_id]))
    const state = cardState(work, [...input.nodes], projection)
    if (!terminalStates.has(state)) return true
    excluded.push({ check_id: check.check_id, work_id: check.work_id, reason: "terminal-work", policy: "web-filtered" })
    return false
  })
  return { checks: kept, excluded }
}

/**
 * Normalize local and bounded remote facts before the existing overview selectors run.
 * This function is pure: it only filters/copies arrays and delegates projection semantics to
 * `overviewFromWorkspace`, `openWorksFromOverview`, and `workspaceActivities`.
 */
export function prepareOverviewInputs(input: OverviewInputPolicy): PreparedOverviewInputs {
  const remoteOverview = input.overview ?? input.remoteOverview ?? null
  const assigned = assignedIds(input)
  const localWorks = filterWorks(input.openWorks, input, assigned)
  const localQuestions = filterQuestions(input.questions, input)
  const localOverview = overviewFromWorkspace({ openWorks: localWorks, bots: input.bots, questions: localQuestions, nodes: input.nodes })
  const overview = scopeOverview(remoteOverview ?? localOverview, input, assigned)
  const projectedWorks = openWorksFromOverview(overview, localWorks)
  const works = filterWorks(projectedWorks, input, assigned)
  const { checks, excluded } = filterChecks(input.checks, works, overview, input)
  const jobs = [...(input.jobs ?? [])]
  const activities = workspaceActivities({
    works,
    questions: localQuestions,
    checks,
    overview,
    jobs,
    answered: input.answered,
    nodes: input.nodes,
    knownBots: input.read === false ? [] : input.bots,
    connected: input.connected,
    read: input.read,
  })
  return {
    works,
    questions: localQuestions,
    checks,
    jobs,
    overview,
    coverage: overview.coverage,
    activities,
    terminalCheckPolicy: input.terminalCheckPolicy,
    excludedTerminalChecks: excluded,
    terminalCheckReasons: excluded,
  }
}
