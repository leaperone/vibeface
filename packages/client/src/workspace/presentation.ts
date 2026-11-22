import type { PendingMessage } from "../chat/types.ts"
import type { ContextProjection, InputReceipt, WorkspaceActivity, WorkspaceActivityGroup, WorkspaceActivityView, WorkspaceDomainState, WorkspaceTask, WorkspaceTaskAction } from "./types.ts"

type ContextSourceKind = ContextProjection["sources"][number]["kind"]

/** Shared labels for the bounded context projection. Keep this exhaustive as protocol kinds grow. */
export const contextSourceLabels = {
  trigger: "本轮交代",
  message: "共享消息",
  work: "工作结果",
  dispatch: "工作指派",
  checkpoint: "历史摘要",
  handoff: "任务交接",
  memory: "持久记忆",
  question: "已知问题",
  attachment: "附件",
} satisfies Record<ContextSourceKind, string>

export function contextSourceSummary(sources: ContextProjection["sources"]) {
  const counts = new Map<string, number>()
  for (const { kind } of sources) counts.set(kind, (counts.get(kind) ?? 0) + 1)
  return Object.entries(contextSourceLabels).flatMap(([kind, label]) => {
    const count = counts.get(kind)
    return count ? [{ kind, label, count }] : []
  })
}

/** The three task sections shared by the Web and Mobile workspace views. */
type WorkspaceTaskGroup = "attention" | "working" | "waiting"

/** Pure semantic state for a workspace task. Renderers choose tone, icon and layout. */
type WorkspaceTaskStateDescriptor = {
  label: string
  attention: boolean
  active: boolean
  group: WorkspaceTaskGroup | null
  allowedActions: readonly WorkspaceTaskAction[]
}

const actionSets = {
  continue: ["continue", "defer", "cancel"] as const satisfies readonly WorkspaceTaskAction[],
  running: ["stop"] as const satisfies readonly WorkspaceTaskAction[],
  none: [] as const satisfies readonly WorkspaceTaskAction[],
} as const

/**
 * Every task state is listed here deliberately. A protocol state added later must update this
 * map before it can be presented by either client.
 */
export const workspaceTaskStateDescriptor = {
  not_started: { label: "未开始", attention: false, active: false, group: "waiting", allowedActions: actionSets.continue },
  running: { label: "执行中", attention: false, active: true, group: "working", allowedActions: actionSets.running },
  waiting_dependencies: { label: "等待依赖", attention: false, active: false, group: "waiting", allowedActions: actionSets.continue },
  waiting_user: { label: "等待你决定", attention: true, active: false, group: "attention", allowedActions: actionSets.continue },
  update_pending: { label: "更新待承接", attention: true, active: false, group: "attention", allowedActions: actionSets.continue },
  stop_pending: { label: "停止待确认", attention: true, active: true, group: "attention", allowedActions: actionSets.none },
  needs_reconciliation: { label: "结果待确认", attention: true, active: false, group: "attention", allowedActions: actionSets.none },
  completed: { label: "已完成", attention: false, active: false, group: null, allowedActions: actionSets.none },
  cancelled: { label: "已取消", attention: false, active: false, group: null, allowedActions: actionSets.none },
  deferred: { label: "稍后处理", attention: false, active: false, group: "waiting", allowedActions: actionSets.continue },
} satisfies Record<WorkspaceTask["state"], WorkspaceTaskStateDescriptor>

/** Return the section for a task state; settled states are not in a live section. */
export function workspaceTaskGroup(taskOrState: Pick<WorkspaceTask, "state"> | WorkspaceTask["state"]): WorkspaceTaskGroup | null {
  const state = typeof taskOrState === "string" ? taskOrState : taskOrState.state
  return workspaceTaskStateDescriptor[state].group
}

/** Count the same task set that a caller passed to its rows. */
export function workspaceTaskCounts(tasks: readonly Pick<WorkspaceTask, "state">[]): Record<WorkspaceTaskGroup, number> {
  const counts: Record<WorkspaceTaskGroup, number> = { attention: 0, working: 0, waiting: 0 }
  for (const task of tasks) {
    const group = workspaceTaskGroup(task)
    if (group) counts[group] += 1
  }
  return counts
}

type InputStatusDescriptor = {
  label: string
  attention: boolean
  active: boolean
}

export const inputDeliveryDescriptor = {
  saved: { label: "本地已保存", attention: false, active: false },
  uploading: { label: "本地已保存，附件准备中", attention: false, active: true },
  submitting: { label: "正在发送给连接的设备", attention: false, active: true },
  accepted: { label: "连接的设备已接收", attention: false, active: false },
  unknown: { label: "是否送达尚未确认", attention: true, active: false },
} satisfies Record<PendingMessage["delivery"], InputStatusDescriptor>

export const inputReceiptDescriptor = {
  entry_accepted: { label: "连接的设备已接收", attention: false, active: false },
  owner_accepted: { label: "执行设备已接纳", attention: false, active: false },
  submitting: { label: "提交已登记，等待执行服务确认", attention: false, active: true },
  provider_accepted: { label: "执行服务已接受输入", attention: false, active: true },
  associated: { label: "已确认回合归属", attention: false, active: false },
  refused: { label: "已拒绝", attention: true, active: false },
  unknown: { label: "补充是否进入当前工作尚未确认", attention: true, active: false },
} satisfies Record<InputReceipt["stage"], InputStatusDescriptor>

export function pendingInputLabel(pending: Pick<PendingMessage, "delivery" | "rejection_code" | "rejected" | "receipt">): string {
  const { delivery, receipt, rejected } = pending
  if (pending.rejection_code === "input_missing") return "这条输入的送达结果仍未确认"
  if (rejected) return `已拒绝 · ${rejected}`
  if (delivery === "unknown" && receipt?.stage === "refused" && receipt.retry_safe) return "重试请求待确认"
  return receipt ? inputReceiptDescriptor[receipt.stage].label : inputDeliveryDescriptor[delivery].label
}

type TaskOverviewSelection = {
  businessTasks: readonly WorkspaceTask[]
  taskCounts: Record<WorkspaceTaskGroup, number>
  groups: Record<WorkspaceActivityGroup, WorkspaceActivity[]>
  visibleActivities: readonly WorkspaceActivity[]
  heldGroups: Record<WorkspaceActivityGroup, number>
}

/** Shared business-task and activity selection; platform adapters supply Bot attribution. */
export function selectTasksOverview(input: {
  workspace: WorkspaceDomainState | null | undefined
  activities: WorkspaceActivityView
  botId: string
  failedWorkIds: ReadonlySet<string>
  includeActivity: (activity: WorkspaceActivity) => boolean
}): TaskOverviewSelection {
  const businessTasks = (input.workspace?.tasks ?? []).filter((task) => input.botId === "all" || task.owner_bot === input.botId)
  const taskCounts = workspaceTaskCounts(businessTasks)
  const knownTaskIds = new Set((input.workspace?.tasks ?? []).map((task) => task.task_id))
  const assignedWorkIds = new Set((input.workspace?.assignments ?? []).flatMap((assignment) => assignment.work_id && knownTaskIds.has(assignment.task_id) ? [assignment.work_id] : []))
  const groups: Record<WorkspaceActivityGroup, WorkspaceActivity[]> = { attention: [], working: [], waiting: [] }
  const heldGroups: Record<WorkspaceActivityGroup, number> = { attention: 0, working: 0, waiting: 0 }
  const visibleActivities: WorkspaceActivity[] = []
  for (const activity of input.activities.activities) {
    if (activity.kind === "work" && activity.work && assignedWorkIds.has(activity.work.work_id)) continue
    if (!input.includeActivity(activity)) continue
    const held = (activity.kind === "work" && activity.work && input.failedWorkIds.has(activity.work.work_id))
      || (activity.kind === "attention" && !("kind" in activity.attention && (activity.attention.kind === "question" || activity.attention.kind === "check")))
    if (held) heldGroups[activity.group] += 1
    else { visibleActivities.push(activity); groups[activity.group].push(activity) }
  }
  return { businessTasks, taskCounts, groups, visibleActivities, heldGroups }
}
