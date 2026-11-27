import type { ScheduleView } from '../workspace/types.ts'

export function quietScheduleRequest(schedule: Pick<ScheduleView, 'schedule_id' | 'name'>): string {
  return '请调整这个 Bot 的定时任务「' + schedule.name + '」（编号：' + schedule.schedule_id + '）的联系约定：只有发现变化、执行失败或需要我决定时才联系我；无变化时明确静默结束并保留执行记录。保持原执行时间、频率和现有权限。修改后向我确认保存的约定和下次执行时间。'
}
export const contactCopy = {
  scope: '只影响这台设备的提醒。任务照常执行，待处理仍会保留。',
  pause: '暂停只停止后续触发，不中断已开始的执行。',
  recurring: '周期任务无变化时可以静默结束，执行记录仍可查看；失败或需要你处理的事项仍会保留。',
  once: '一次性提醒会返回结果，不使用“无变化”静默。',
  request: '这只是可编辑的修改请求。由你发送后，再以 Bot 回复和保存的约定为准。',
}
