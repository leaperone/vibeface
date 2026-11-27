import type { Attachment } from '../chat/types.ts'

export type FeedbackSource = { scope_id: string } & (
  | { kind: 'task_result'; task_id: string; revision: number; event_hash: string }
  | { kind: 'work_result'; work_id: string; event_hash: string }
  | ({ kind: 'attachment' } & Attachment)
)

export function validFeedbackSource(source: FeedbackSource) {
  return /^[a-f0-9]{64}$/.test(source.kind === 'attachment' ? source.hash : source.event_hash)
    && (source.kind !== 'task_result' || Number.isSafeInteger(source.revision) && source.revision > 0)
}

export function feedbackLabel(source: FeedbackSource) {
  return source.kind === 'task_result' ? `第 ${source.revision} 版任务结果` : source.kind === 'work_result' ? '这个工作结果' : `附件 ${source.name}`
}

export function feedbackReference(source: FeedbackSource) {
  return source.kind === 'task_result' ? `任务 ${source.task_id} · 第 ${source.revision} 版\n结果事件 ${source.event_hash}`
    : source.kind === 'work_result' ? `工作 ${source.work_id}\n结果事件 ${source.event_hash}`
    : `附件 ${source.name} · ${source.size} 字节\n文件指纹 ${source.hash}`
}

export function feedbackMessage(source: FeedbackSource, conversationId: string, text: string) {
  if (!validFeedbackSource(source) || !text.trim()) throw new Error('请确认成果版本并填写反馈。')
  return `这是对已有成果的反馈，请先讨论反馈，不继续或重新执行原任务。\n反馈会话 ${conversationId}\n引用范围 ${source.scope_id}\n${feedbackReference(source)}\n\n我的反馈：\n${text.trim()}`
}
