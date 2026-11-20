import { cardState, workspaceActivities } from "./labels.ts"
import { openWorksFromOverview, overviewFromWorkspace } from "./model.ts"
import { questionOpen } from "./questions.ts"
import type {
  BotRecord,
  NodeInfo,
  OpenWorkCard,
  OverviewBot,
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
  /** Assignment rows are admitted only when their task_id is present in the task snapshot. */
  assignments?: readonly Pick<WorkspaceAssignment, "work_id" | "task_id">[]
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
  const knownTaskIds = new Set([
    ...(input.tasks ?? []).map((task) => task.task_id),
    ...(input.workspaceState?.tasks ?? []).map((task) => task.task_id),
  ])
  const validAssignmentWorkIds = (assignments: readonly Pick<WorkspaceAssignment, "work_id" | "task_id">[]) => assignments.flatMap((assignment) => {
    if (!assignment.work_id || !knownTaskIds.has(assignment.task_id)) return []
    return [assignment.work_id]
  })
  const values = [
    ...validAssignmentWorkIds(input.assignments ?? []),
    ...validAssignmentWorkIds(input.workspaceState?.assignments ?? []),
  ]
  return new Set(values)
}

function scopeOverview(overview: WorkspaceOverview, input: OverviewInputPolicy, assigned: ReadonlySet<string>, bots: readonly (BotRecord | OverviewBot)[]): WorkspaceOverview {
  if ((!input.botFilter || input.botFilter === "all") && assigned.size === 0) return overview
  const taskVisible = (task: WorkspaceOverview["tasks"][number]) => !assigned.has(task.work_id ?? "") && botMatches(input.botFilter, task.conversation_id, bots, task.bot?.bot_id)
  return {
    ...overview,
    tasks: overview.tasks.filter(taskVisible),
    questions: overview.questions.filter((question) => botMatches(input.botFilter, question.conversation_id, bots, question.bot?.bot_id)),
    jobs: overview.jobs.filter((job) => !job.conversation_id || botMatches(input.botFilter, job.conversation_id, bots)),
  }
}

function botMatches(botFilter: string | undefined, conversationId: string, bots: readonly (BotRecord | OverviewBot)[], explicitBotId?: string): boolean {
  if (!botFilter || botFilter === "all") return true
  return explicitBotId === botFilter || bots.some((bot) => bot.bot_id === botFilter && bot.conversation_id === conversationId)
}

function filterWorks(works: readonly OpenWorkCard[], input: OverviewInputPolicy, assigned: ReadonlySet<string>, bots: readonly (BotRecord | OverviewBot)[]): OpenWorkCard[] {
  return works.filter((work) => !assigned.has(work.work_id) && botMatches(input.botFilter, work.conversation_id, bots))
}

function filterQuestions(questions: readonly Question[], input: OverviewInputPolicy, bots: readonly (BotRecord | OverviewBot)[]): Question[] {
  return questions.filter((question) => questionOpen(question) && botMatches(input.botFilter, question.conversation_id, bots, question.bot?.bot_id))
}

function filterChecks(
  checks: readonly PendingCheck[],
  works: readonly OpenWorkCard[],
  overview: WorkspaceOverview,
  input: OverviewInputPolicy,
  assigned: ReadonlySet<string>,
): { checks: PendingCheck[]; excluded: ExcludedTerminalCheck[] } {
  const excluded: ExcludedTerminalCheck[] = []
  const projectionByKey = new Map(overview.tasks.flatMap((task) => {
    if ((task.kind !== "work" && task.kind !== "dispatch") || !task.work_id) return []
    return [[JSON.stringify([task.conversation_id, task.work_id]), task] as const]
  }))
  const scopedBots: readonly (BotRecord | OverviewBot)[] = [...input.bots, ...overview.bots]
  const checkIsTerminal = (work: OpenWorkCard) => {
    if (input.terminalCheckPolicy !== "web-filtered") return false
    const projection = projectionByKey.get(JSON.stringify([work.conversation_id, work.work_id]))
    return terminalStates.has(cardState(work, [...input.nodes], projection))
  }
  const kept = checks.flatMap((check): PendingCheck[] => {
    if (check.kind === "process") return [check]
    if (check.kind === "work") {
      if (!botMatches(input.botFilter, check.conversation_id, scopedBots) || assigned.has(check.work_id)) {
        const work = works.find((candidate) => checkMatchesWork(check, candidate))
        if (work && checkIsTerminal(work)) excluded.push({ check_id: check.check_id, work_id: check.work_id, reason: "terminal-work", policy: "web-filtered" })
        return []
      }
      const work = works.find((candidate) => checkMatchesWork(check, candidate))
      if (work && checkIsTerminal(work)) {
        excluded.push({ check_id: check.check_id, work_id: check.work_id, reason: "terminal-work", policy: "web-filtered" })
        return []
      }
      return [check]
    }
    const scopedWorks = check.works.filter((item) => !assigned.has(item.work_id) && botMatches(input.botFilter, item.conversation_id, scopedBots))
    const visibleWorks = input.terminalCheckPolicy === "web-filtered"
      ? scopedWorks.filter((item) => {
        const work = works.find((candidate) => candidate.conversation_id === item.conversation_id && candidate.work_id === item.work_id && candidate.target_node === item.target_node)
        if (!work) return true
        if (!checkIsTerminal(work)) return true
        excluded.push({ check_id: check.check_id, work_id: item.work_id, reason: "terminal-work", policy: "web-filtered" })
        return false
      })
      : scopedWorks
    return visibleWorks.length ? [{ ...check, works: visibleWorks }] : []
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
  const availableBots: readonly (BotRecord | OverviewBot)[] = [...input.bots, ...(remoteOverview?.bots ?? [])]
  const assigned = assignedIds(input)
  const localWorks = filterWorks(input.openWorks, input, assigned, availableBots)
  const localQuestions = filterQuestions(input.questions, input, availableBots)
  const localOverview = overviewFromWorkspace({ openWorks: localWorks, bots: input.bots, questions: localQuestions, nodes: input.nodes })
  const rawOverview = remoteOverview ?? localOverview
  const allProjectedWorks = openWorksFromOverview(rawOverview, filterWorks(input.openWorks, input, new Set(), availableBots))
  const overview = scopeOverview(rawOverview, input, assigned, availableBots)
  const projectedWorks = openWorksFromOverview(overview, localWorks)
  const works = filterWorks(projectedWorks, input, assigned, availableBots)
  const { checks, excluded } = filterChecks(input.checks, filterWorks(allProjectedWorks, input, new Set(), availableBots), rawOverview, input, assigned)
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
