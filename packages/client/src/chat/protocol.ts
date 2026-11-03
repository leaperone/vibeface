import { botReadiness, inputText } from '../workspace/model.ts'
import { newId } from '../workspace/storage.ts'
import type { MessageRequest, PendingMessage, TranscriptMessage } from './types.ts'
import { failureText } from '../lib/errors.ts'
import { asProvider, providerNames, probeIssueText } from '../lib/node.ts'
import type { ManagerJob, ProviderStage, TurnRuntime, NodeInfo } from '../workspace/types.ts'
import type { FailureKind } from '../lib/errors.ts'

/** An empty chat's first messages: project questions for a bot with a folder, otherwise what any bot can do from chat. */
export const starters = { project: ['看看这个项目的结构', '帮我检查依赖', '最近改了什么？'], general: ['你能帮我做什么？', '每天早上 9 点提醒我', '记住：回复用中文'] }

type SendGate = {
  hasConversation: boolean
  loaded: boolean
  busy: boolean
}
export function roleLabel(role: string): string {
  if (role === 'user') return '你'
  if (role === 'assistant') return 'Bot'
  return role
}
export function canSend(input: SendGate): boolean {
  return Boolean(input.hasConversation && input.loaded && !input.busy)
}
/** Whether any device can run the bot now: `starting` while one comes up, `offline` when the only ones that could are away, `none` when none is set up to. */
export type BotReach = 'ready' | 'unverified' | 'starting' | 'needs_setup' | 'offline' | 'none'
/** A device that only needs setup is fixable now, so it comes before one that is offline. */
export function botReach(nodes: NodeInfo[]): BotReach {
  if (nodes.some((node) => botReadiness(node) === 'ready')) return 'ready'
  if (nodes.some((node) => botReadiness(node) === 'unverified')) return 'unverified'
  if (nodes.some((node) => botReadiness(node) === 'starting')) return 'starting'
  if (nodes.some((node) => botReadiness(node) === 'needs_setup')) return 'needs_setup'
  return nodes.some((node) => !node.revoked && !node.online && node.capabilities?.manager) ? 'offline' : 'none'
}
export const botCanSend = (reach: BotReach) => reach === 'ready' || reach === 'unverified'

export const canRetryJob = (job: ManagerJob) => job.retryable === true && job.runtime?.execution === 'not_started' && !job.pending_check && job.state === 'failed'
export function runtimeLabel(runtime: TurnRuntime): string {
  const model = runtime.model ? `提交模型 ${runtime.model}` : 'AI 服务默认模型，实际模型未确认'
  const effort = runtime.reasoning_effort ? `提交推理强度 ${runtime.reasoning_effort}` : '推理强度由 AI 服务决定'
  const switched = runtime.switched_from ? `，由 ${providerNames[runtime.switched_from.provider]} 的 ${runtime.switched_from.model ?? '默认模型'} 切换` : ''
  const stages: Record<ProviderStage, string> = { validate: '检查设置', install: '检查程序', launch: '启动程序', authentication: '检查登录', model_list: '检查模型', session_start: '开始会话', turn_start: '提交回合', run: '执行回合', read_back: '查询结果', interrupt: '中断回合', compact: '整理上下文', handoff: '交接上下文', record: '记录结果' }
  const reason = runtime.switch_reason ? `，原因 ${failureText(runtime.switch_reason) || 'AI 服务未能启动'}` : ''
  return `${providerNames[runtime.provider]} · ${model} · ${effort} · ${stages[runtime.stage]}${switched}${reason}`
}
export function botProviderNotice(nodes: NodeInfo[], owner?: string | null): string {
  const node = nodes.find((item) => !item.revoked && item.device_id === owner && item.capabilities?.manager)
    ?? nodes.find((item) => !item.revoked && item.online && item.capabilities?.manager)
    ?? nodes.find((item) => !item.revoked && item.capabilities?.manager)
  if (!node?.capabilities?.manager) return ''
  const provider = asProvider(node.capabilities.manager)
  const prefix = `${provider ? providerNames[provider] : node.capabilities.manager} · ${node.name || '运行设备'}`
  if (!node.online) return `${prefix} 离线，先恢复设备连接，再查询回复结果。`
  const readiness = node.capabilities.manager_readiness
  if (readiness && 'issue' in readiness && provider) return `${prefix} · ${probeIssueText(provider, readiness.issue)}。检查状态${readiness.state === 'unverified' ? '尚未确认' : '需设置'}。`
  if (node.capabilities.manager_state === 'unverified') return `${prefix} 尚未确认可用，可发送消息或在设备设置检查。`
  if (node.capabilities.manager_state === 'needs_setup') return `${prefix} 需设置，请在运行设备检查登录和模型。`
  if (node.capabilities.manager_state === 'starting') return `${prefix} 正在启动，请稍后查询。`
  return ''
}

/** The device a person goes to while the bot cannot reply: the one to set up, else the one that is offline. */
export function reachDevice(nodes: NodeInfo[], reach: BotReach): string | undefined {
  if (reach === 'needs_setup') return nodes.find((node) => botReadiness(node) === 'needs_setup')?.device_id
  if (reach === 'offline') return nodes.find((node) => !node.revoked && !node.online && node.capabilities?.manager)?.device_id
  return undefined
}
/** A device that exists but is not ready yet may become ready on its own; with none, setting one up rereads it. */
export const awaitsDevice = (reach: BotReach) => reach === 'starting' || reach === 'offline' || reach === 'needs_setup'
/** Why the box cannot send yet, short enough to stand in for its placeholder. */
export function composerHint(input: Omit<SendGate, 'busy'> & { bot: BotReach }): string {
  if (!input.hasConversation) return ''
  if (input.bot === 'starting') return 'Bot 正在启动'
  if (input.bot === 'offline') return '能运行 Bot 的设备离线了'
  if (input.bot === 'needs_setup') return '运行 Bot 的设备需设置'
  if (input.bot === 'none') return '还没设置运行 Bot 的设备'
  if (!input.loaded) return '稍等'
  return ''
}
export function prepareSend(input: {
  conversationId: string
  text: string
  pending: PendingMessage | null
  reportResults?: boolean
  /** It carries files, which are a message on their own. */
  files?: boolean
}): { ok: true; pending: PendingMessage; body: MessageRequest } | { ok: false; error: string } {
  let text: string
  try { text = input.files && !input.text.trim() ? '' : inputText(input.text, '消息内容', 65536) }
  catch (caught) { return { ok: false, error: caught instanceof Error ? caught.message : '消息内容无效。' } }
  if (input.pending && (input.pending.text !== text || input.pending.conversation_id !== input.conversationId)) {
    return { ok: false, error: '上一条消息还没确认送达，先处理它再发新的。' }
  }
  const pending = input.pending ?? {
    message_id: newId(), conversation_id: input.conversationId, text,
    report_results: input.reportResults ?? true,
  }
  return { ok: true, pending, body: {
    conversation_id: pending.conversation_id, message_id: pending.message_id, text: pending.text,
    report_results: pending.report_results ?? true,
  } }
}
/** The Node admitted this exact message; a retry of the same message_id returns the same receipt. */
export function isQueuedReceipt(value: unknown, pending: PendingMessage): boolean {
  return value !== null && typeof value === 'object' && 'accepted' in value && value.accepted === true && 'message_id' in value && value.message_id === pending.message_id
}
// These end a reply before its AI service starts, so the reply did nothing.
const beforeStart = new Set<FailureKind>(['missing_key', 'missing_binary', 'execution_disabled', 'no_model', 'no_bot', 'writer_busy'])
/**
 * What 重试 sends for a reply that failed or was stopped. Only a reply that provably did nothing, by its failure and by leaving no
 * task or change behind, gets its message again as it was; any other may have written something, so the bot checks first.
 */
export function retryText(text: string, failure: FailureKind | undefined, traces: boolean): string {
  if (failure && beforeStart.has(failure) && !traces) return text
  return `上次没回完，可能已经做了一部分。先看看做过什么，做过的不要重做，再接着完成${text.trim() ? `：\n${text}` : '。'}`
}
export function replyStatusLabel(status: string | undefined): string {
  if (status === 'stopped') return '已中断'
  if (status === 'interrupted') return '已中断'
  if (status === 'failed') return '失败'
  return ''
}
export function messageKey(message: TranscriptMessage): string { return message.event_hash }

export function isSubmitKey(event: { key: string; shiftKey: boolean; isComposing: boolean; keyCode: number }): boolean {
  return event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229
}
