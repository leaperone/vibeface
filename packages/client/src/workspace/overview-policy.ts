import { cardState, workspaceActivities } from "./labels.ts"
import { openWorksFromOverview, overviewFromWorkspace } from "./model.ts"
import type {
  BotRecord,
  NodeInfo,
  OpenWorkCard,
  OverviewTask,
  PendingCheck,
  Question,
  WorkspaceActivityJob,
  WorkspaceOverview,
} from "./types.ts"

type OverviewInputs = {
  openWorks: readonly OpenWorkCard[]
  bots: readonly BotRecord[]
  questions: readonly Question[]
  nodes: readonly NodeInfo[]
  checks: readonly PendingCheck[]
  jobs?: readonly WorkspaceActivityJob[]
  overview?: WorkspaceOverview | null
  answered?: Readonly<Record<string, string>>
  read: boolean
  connected: boolean
  terminalCheckPolicy: "web-filtered" | "mobile-inclusive"
}

const workKey = (conversationId: string, workId: string) => JSON.stringify([conversationId, workId])
const terminalStates = new Set(["completed", "failed", "interrupted", "rejected"])

function pruneTerminalChecks(
  checks: readonly PendingCheck[],
  works: readonly OpenWorkCard[],
  projections: readonly OverviewTask[],
  nodes: NodeInfo[],
): PendingCheck[] {
  const projected = new Map<string, Extract<OverviewTask, { kind: "work" | "dispatch" }>>()
  for (const task of projections) {
    if ((task.kind === "work" || task.kind === "dispatch") && task.work_id) {
      projected.set(workKey(task.conversation_id, task.work_id), task)
    }
  }
  const needsConfirmation = (check: Pick<OpenWorkCard, "conversation_id" | "work_id" | "target_node">) => {
    const work = works.find((candidate) => candidate.conversation_id === check.conversation_id
      && candidate.work_id === check.work_id && candidate.target_node === check.target_node)
    return !work || !terminalStates.has(cardState(work, nodes, projected.get(workKey(work.conversation_id, work.work_id))))
  }
  return checks.flatMap((check): PendingCheck[] => {
    if (check.kind === "process") return [check]
    if (check.kind === "work") return needsConfirmation(check) ? [check] : []
    const remaining = check.works.filter(needsConfirmation)
    return remaining.length ? [{ ...check, works: remaining }] : []
  })
}

/** Keep the complete snapshot available for counts and let each screen apply its selection. */
export function prepareOverviewInputs(input: OverviewInputs) {
  const local = overviewFromWorkspace(input)
  const overview = input.overview ?? local
  const works = openWorksFromOverview(overview, input.openWorks)
  const checks = input.terminalCheckPolicy === "web-filtered"
    ? pruneTerminalChecks(input.checks, works, [...local.tasks, ...overview.tasks], [...input.nodes])
    : input.checks
  const activities = workspaceActivities({
    works,
    questions: input.questions,
    checks,
    overview,
    jobs: input.jobs,
    answered: input.answered,
    nodes: input.nodes,
    knownBots: input.read ? input.bots : [],
    connected: input.connected,
    read: input.read,
  })
  return { works, overview, activities }
}
