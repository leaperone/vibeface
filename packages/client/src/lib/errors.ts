import { nodeJoinCopy } from '../identity/copy.ts'

/**
 * The one place backend and protocol errors become words a person reads (docs/design/web-ux.md).
 * Nodes answer in English; the page never shows that text as the message, only under 详细信息.
 */
const codes: Record<string, string> = {
  target_not_member: '那台电脑不在我的设备里，访问地址不会转发给它。',
  target_unreachable: '访问地址连不上那台电脑。它可能关机或断网了，稍后会自动重试。',
  codex_missing: '没找到这台电脑上的 Codex 目录。装好 Codex，或手动填写 Codex 目录。',
}

const malformed = /invalid|is required|must be|missing/i
export const spentInviteText = '这条加入链接已用过或已过期。请在已有设备上点“添加设备”重新生成。'
/** A join link the Node will not take again: already answered, or past its time, in the Node's words or ours. */
export const spentInvite = new RegExp(`ticket has already been used|ticket is unknown or already expired|ticket has expired|^(${[spentInviteText, nodeJoinCopy.used, nodeJoinCopy.expired].join('|')})$`, 'i')
const patterns: [RegExp, string][] = [
  [/face changed since you saw it/i, 'Bot 的长相刚在别处改过，这次没有改，已显示最新的长相。'],
  [/artifact is not UTF-8 text/i, '这个文件不是文字，下载后再打开。'],
  [/log has forked/i, '这台电脑的数据被复制过，已暂停发送。去待处理看看。'],
  [/read-only until you confirm/i, '这台电脑暂时只读。去待处理确认一下就能发送。'],
  [/aligning with other Nodes/i, '正在和你的其他设备同步，稍后就能发送。'],
  [/is set by Vibeface/i, '这个变量由 Vibeface 自己设置，换个名字。'],
  [/variable names use/i, '变量名只能用字母、数字和下划线，且不能以数字开头。'],
  [/variable value must be|variables per provider/i, '变量值不能为空，也不能太长。'],
  [/delivery service is disabled|service outcome is unknown/i, '对方或中转设备没有回应，可能还没打开文件传输。确认后再试，不会重复发送。'],
  [/outcome may be unknown|timed out/i, '设备没有及时回复，结果未知。请先确认结果，不要重复提交。'],
  [/no configured management SDK/i, '这台电脑没开 Bot。去设备页的「Bot 与任务」里选一个。'],
  [/question was closed/i, '这个请求已失效，不能再回答。'],
  [/answered meanwhile|no App Server request is waiting/i, '这个请求已经回答过或已过期。'],
  [/no rollout found/i, 'Codex 新对话还没准备好，任务没有开始。请再派一次。'],
  [/has no sessions folder/i, '这台电脑的 Codex 还没有会话记录。先用 Codex 跑一次对话，再回来重试。'],
  [/explicit App Server socket/i, '还没填 Codex 的连接路径。'],
  [/App Server socket must be an absolute path/i, '连接路径要是绝对路径。'],
  [/explicit absolute CODEX_HOME/i, '由 Vibeface 启动前，先填 Codex 目录。'],
  [/Codex SDK|Codex rejected|Codex turn failed|App Server/i, 'Codex 没能完成这次回复。请再发一次；一直不行就去设备页检查 Codex。'],
  [/peer (did not answer|is unreachable|connection closed|closed the stream|stream failed)|target connection closed|client target refused/i, '另一台设备没有回应，可能关机或断网了。等它上线后再试。'],
  [/client disconnected|client channel idle|membership state closed/i, '连接断开了，正在重新连接。'],
  [/Failed to fetch|NetworkError|Load failed/i, '连不上设备，请检查网络后再试。'],
  [/network has not started|maintenance lease|another maintenance/i, '这台电脑正在启动或维护，请稍后再试。'],
  [/another personal network|wrong personal network|wrong membership network|unknown network|network mismatch/i, '这台电脑属于另一组设备。'],
  [/not a member of (the|this) personal network|client identity missing/i, '这台设备还不在我的设备里。'],
  [/already a member or was revoked/i, '这台设备已经加入过或被移除过，这个链接用不了。'],
  [/too many devices are joining|too many pending invites/i, '同时加入的设备太多，请稍后再试。'],
  [/has not confirmed its short code/i, '新设备还没显示安全码，等它显示后再比对。'],
  [/only the manager that started the confirmation/i, '请在发出加入链接的那台设备上批准。'],
  [/join request (not found|vanished|can no longer|is not waiting|belongs to another)|join completed with another grant|different nonce/i, '这个加入请求已处理或已过期。请重新生成加入链接。'],
  [/this recovery removes this Node/i, '这次找回会移除你正连着的设备。请换一台要保留的设备打开。'],
  [/replaced by a newer recovery/i, '这个恢复文件已作废。请用最近一次下载的恢复文件。'],
  [/conflicting recovery checkpoint|recovery epoch rollback/i, '这组设备已经用另一份恢复文件找回过。请用最近一次下载的恢复文件。'],
  [/another recovery is waiting/i, '已有一次找回在等待生效。'],
  [/stopped this recovery/i, '这次找回已被拦下。'],
  [/needs at least one management device/i, '至少要留一台管理设备。'],
  [/no recovery is waiting/i, '没有正在等待的找回，可能已生效或已被拦下。'],
  [/signed too long ago/i, '这次找回太旧了，请重新发起。'],
  [/can no longer be stopped/i, '这次找回马上就要生效，来不及拦下了。'],
  [/too far from this Node's clock/i, '这台设备的时间和所连电脑差得太多。请校准时间后再试。'],
  [/write needs the signature|signed by another device|client_id differs/i, '这台设备的身份校验没通过。请重新打开后重试。'],
  [/conversation moved meanwhile/i, '这个对话刚换过负责的设备，已刷新。请确认后再试。'],
  [/management reply changed/i, '回复状态已变化，请刷新后再停止。'],
  [/scheduled on another device/i, '这条回复由另一台设备负责，请到那台设备上停止。'],
  [/already answered or expired|executor restarted; this request expired/i, '这个请求已经回答过或已过期。'],
  [/not waiting for an answer/i, '这个任务现在不需要回答。'],
  [/answer every question|answers? (is|are) (missing|required)|answers must be text/i, '请回答每个问题后再提交。'],
  [/message ID reused|work ID reused|conversation ID already exists/i, '这个编号已用于不同的内容，没有重复提交。'],
  [/send window for this work is over/i, '这个任务等得太久，没有开始。需要的话请再发一次。'],
  [/schedule this work runs for is paused|the schedule was deleted/i, '这个定时任务已暂停或删除。'],
  [/has no bot to take the task/i, '那台设备还没设置 Bot。'],
  [/has not reached this device|not stored on this device|not on this device|not found on this device/i, '内容还没同步到这台电脑，稍后再试。'],
  [/not reached the scheduling Node/i, '消息还没送到负责这个对话的设备，送到后会自动排队。'],
  [/conversation has no scheduling Node|does not schedule the conversation/i, '这个对话不由这台电脑负责。'],
  [/conversation not found/i, '这台电脑上还没有这个对话，可能还在同步。'],
  [/work not found|no work was dispatched/i, '找不到这个任务，可能还在同步。'],
  [/pending check not found|only unverified work/i, '这项确认已处理或不存在。'],
  [/scheduler inbox is full/i, '排队的消息太多，这条没有收下。请稍后再发。'],
  [/attachment|blob/i, '附件还没同步到这台电脑，或校验没通过。'],
  [/artifact/i, '这个产物现在读不了。'],
  [/outside (the execution Node's )?configured roots/i, '这个文件夹不在这台电脑允许的文件夹里。去设备页添加。'],
  [/set a webhook (URL|secret) first/i, '先填好 Webhook 地址和密钥。'],
  [/webhook must use HTTPS/i, 'Webhook 地址要以 https 开头。'],
  [/nothing to change|is unchanged/i, '内容没有变化。'],
  [/project root|execution path|workspace must be|workspace does not exist|manager workspace/i, '文件夹不存在，或不是这台电脑上的绝对路径。'],
  [/No such file or directory|ENOENT|command not found|Missing script|cannot find (the )?(file|path|module)/i, '找不到需要的文件、脚本或命令。确认它在这台电脑上，再让 Bot 试。'],
  [/permission denied|EACCES|operation not permitted/i, '这台电脑上没有权限读写需要的文件。'],
  [/coordination Agent|manager provider|manager program/i, '这台电脑上的 Bot 还没配置好。'],
  [/only the user|requires the user|agents cannot|Agent tokens/i, '只有你自己能做这个操作，Bot 不能代办。'],
  [/management client|management permission|management control/i, '只有管理设备能做这个操作。'],
  [/local browser (session|cookie|access)|local (cookie|token) missing|session cookie|local session identity/i, '本机会话已失效。请在这台电脑上重新打开。'],
  [spentInvite, spentInviteText],
  [/invite link|ticket/i, '加入链接无效或不完整。请重新复制。'],
  [/already belongs to a personal network|already initialized/i, '这台电脑已经加入了一组设备。'],
  [/revoked/i, '这台设备已被移除。'],
  [/recovery (key|proof)/i, '恢复文件和这组设备对不上，或这次找回已过期。'],
  [/signature|proof|handshake|identity/i, '设备身份校验没通过，已断开。'],
  [/too large|too long|exceeds|limit/i, '内容超过了大小限制。'],
  [malformed, '请求内容不完整或格式不对。请刷新页面后重试。'],
]

const chinese = /[一-鿿]/

/** Chinese stays as written; known backend text and codes are translated; the rest names the failed action. */
export function humanError(raw: string, code?: unknown, action = '这次操作'): string {
  if (typeof code === 'string' && codes[code]) return codes[code]
  if (chinese.test(raw)) return raw
  return patterns.find(([pattern]) => pattern.test(raw))?.[1] ?? `${action}没有成功，请再试一次。`
}

/** Known backend text in the reader's words, or nothing: for a log line, where a guess would mislead. */
export function knownError(raw: string): string | undefined {
  if (chinese.test(raw)) return raw
  return patterns.find(([pattern]) => pattern !== malformed && pattern.test(raw))?.[1]
}

/** An external chat test as people read it: it arrived, or why not. */
export function webhookTestResult(data: { ok?: boolean; status?: number | null; error?: string | null }) {
  if (data.ok === true) return { ok: true, text: '能收到' }
  return { ok: false, text: data.error ? humanError(data.error, undefined, '这次测试') : data.status ? `对方没收下（${data.status}）` : '没有响应' }
}

/**
 * A task's own failure: its agent's words are not a page request, so advice to refresh the page would mislead.
 * Unknown text keeps its first line under the message, since trying again may not help and the cause is the clue.
 */
export function taskError(raw: string): string {
  if (chinese.test(raw)) return raw
  const known = patterns.find(([pattern]) => pattern !== malformed && pattern.test(raw))?.[1]
  if (known) return known
  const line = raw.trim().split('\n')[0].trim()
  return line ? `这个任务没有成功。\n${line.length > 80 ? `${line.slice(0, 80)}…` : line}` : '这个任务没有成功。'
}

/** Why an execution failed, as the Node names it; its original text stays in 技术详情. */
export type FailureKind = 'missing_key' | 'auth_failed' | 'quota' | 'missing_binary' | 'execution_disabled' | 'no_model' | 'writer_busy'
  | 'rejected' | 'transport' | 'timed_out' | 'restarted' | 'lost' | 'unknown'
const settings = '去「设备 → Bot 与任务」'
const failures: Record<FailureKind, string> = {
  missing_key: `AI 服务缺少密钥。${settings}填好后再试。`,
  auth_failed: `AI 服务登录没通过。${settings}检查密钥后再试。`,
  quota: 'AI 服务的用量到上限了。等额度恢复后再试。',
  missing_binary: `这台电脑上找不到 AI 服务的程序。${settings}检查。`,
  execution_disabled: `这台电脑上这个 AI 服务没开执行任务。${settings}打开。`,
  no_model: `没选模型，或 AI 服务不认识这个模型。${settings}选一个。`,
  writer_busy: '另一个进程正在写这个会话。等它结束后再试。',
  rejected: 'AI 服务拒绝了这次请求，没有执行。',
  transport: '连不上 AI 服务，或连接中途断了。稍后再试。',
  timed_out: 'AI 服务没有及时响应，结果未知。',
  restarted: '设备在中途重启了，这次没有重做。',
  lost: '执行的进程意外结束，不确定做完了没有。先看看有没有改动，再决定要不要重发。',
  unknown: '',
}
export const failureKinds = Object.keys(failures) as FailureKind[]
/** The words for a failure kind; empty for `unknown`, whose caller words it from what it knows. */
export function failureText(kind: FailureKind | null | undefined): string {
  return kind ? failures[kind] : ''
}

/** Why a reply failed, in the reader's words: its kind, else the device's own text, else a plain line. */
export function replyFailure(kind: FailureKind | null | undefined, detail?: string): string {
  return failureText(kind) || humanError(detail ?? '', undefined, '这次回复')
}

/** The untranslated text, for 详细信息; empty when the shown message already is the original. */
export function rawError(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error ?? '')
  return chinese.test(raw) ? '' : raw
}

/** Why a task failed, in one line: its kind's words, else the translated cause; empty when nothing says. */
export function workFailure(work: { failure?: FailureKind; failure_reason: string | null }): string {
  return failureText(work.failure) || (work.failure_reason ? taskError(work.failure_reason).split('\n').at(-1) ?? '' : '')
}

/**
 * Whether a failure may still have done its work: the AI service or device stopped answering, or its process ended
 * unseen. Writes are not repeated on an unknown outcome, so the next step is checking the result, never a plain retry.
 */
export function outcomeUnknown(kind: FailureKind | null | undefined, detail?: string | null): boolean {
  return kind === 'timed_out' || kind === 'lost' || /outcome may be unknown|timed out/i.test(detail ?? '')
}
