/**
 * The one place backend and protocol errors become words a person reads (docs/design/web-ux.md 4.7).
 * Nodes answer in English; the page never shows that text as the message, only under 详细信息.
 */
const codes: Record<string, string> = {
  target_not_member: '这台设备不在我的设备里，访问地址不会转发给它。',
  target_unreachable: '访问地址连不上这台设备。它可能关机或断网了，稍后会自动重试。',
}

const patterns: [RegExp, string][] = [
  [/artifact is not UTF-8 text/i, '这个文件不是文字，下载后再打开。'],
  [/log has forked/i, '这台设备的数据被复制过，已暂停发送。去待处理看看。'],
  [/read-only until you confirm/i, '这台设备暂时只读。去待处理确认一下就能发送。'],
  [/aligning with other Nodes/i, '正在和你的其他设备同步，稍后就能发送。'],
  [/Missing environment variable|CURSOR_API_KEY is (required|missing)/i, 'AI 服务缺少密钥。去设备页的 AI 设置里填上。'],
  [/Codex execution is turned off/i, '这台设备的 Codex 没开执行任务。去设备页的 AI 设置里打开。'],
  [/is set by Vibeface/i, '这个变量由 Vibeface 自己设置，换个名字。'],
  [/variable names use/i, '变量名只能用字母、数字和下划线，且不能以数字开头。'],
  [/variable value must be|variables per provider/i, '变量值不能为空，也不能太长。'],
  [/Cursor SDK bridge did not start within/i, 'Cursor 组件启动超时，这次回复没有开始。请稍后再发一次。'],
  [/completion is unknown|reconcile its outcome/i, 'Bot 中途断开，结果未知。先看看有没有回复，再决定要不要重发。'],
  [/outcome may be unknown|timed out/i, '设备没有及时回复，结果未知。请先确认结果，不要重复提交。'],
  [/Cursor SDK requires (an explicit )?model/i, 'Cursor 还没选模型。去设备页的 AI 设置里选一个。'],
  [/no configured management SDK/i, '这台设备没开 Bot。去设备页的 AI 设置里选一个。'],
  [/configured executable was not found/i, '这台设备上的 Bot 程序找不到了。去设备页检查 AI 设置。'],
  [/Cursor SDK|cursor-sdk-bridge/i, 'Cursor 没能完成这次回复。请再发一次；一直不行就去设备页检查 Cursor。'],
  [/answered meanwhile|no App Server request is waiting/i, '这个请求已经回答过或已过期。'],
  [/usage limit|rate limit|quota|too many requests/i, 'AI 服务的用量到上限了。等额度恢复后让 Bot 再试。'],
  [/no rollout found/i, 'Codex 新对话还没准备好，任务没有开始。请再派一次。'],
  [/Codex SDK|Codex rejected|App Server/i, 'Codex 没能完成这次回复。请再发一次；一直不行就去设备页检查 Codex。'],
  [/peer (did not answer|is unreachable|connection closed|closed the stream|stream failed)|target connection closed|client target refused/i, '另一台设备没有回应，可能关机或断网了。等它上线后再试。'],
  [/client disconnected|client channel idle|membership state closed/i, '连接断开了，正在重新连接。'],
  [/Failed to fetch|NetworkError|Load failed/i, '连不上设备，请检查网络后再试。'],
  [/network has not started|maintenance lease|another maintenance/i, '这台设备正在启动或维护，请稍后再试。'],
  [/another personal network|wrong personal network|wrong membership network|unknown network|network mismatch/i, '这台设备属于另一组设备。'],
  [/not a member of (the|this) personal network|client identity missing/i, '这个浏览器还不在我的设备里。'],
  [/too many devices are joining|too many pending invites/i, '同时加入的设备太多，请稍后再试。'],
  [/has not confirmed its short code/i, '新设备还没显示安全码，等它显示后再比对。'],
  [/only the manager that started the confirmation/i, '请在发出加入链接的那台设备上批准。'],
  [/join request (not found|vanished|can no longer|is not waiting|belongs to another)|join completed with another grant|different nonce/i, '这个加入请求已处理或已过期。请重新生成加入链接。'],
  [/this recovery removes this Node/i, '这次找回会移除你正连着的设备。请换一台要保留的设备打开。'],
  [/replaced by a newer recovery/i, '这个恢复文件已作废。请用最近一次下载的恢复文件。'],
  [/conflicting recovery checkpoint|recovery epoch rollback/i, '这组设备已经用另一份恢复文件恢复过。请用最近一次下载的恢复文件。'],
  [/write needs the signature|signed by another device|client_id differs/i, '这个浏览器的身份校验没通过。请刷新页面后重试。'],
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
  [/has not reached this device|not stored on this device|not on this device|not found on this device/i, '内容还没同步到这台设备，稍后再试。'],
  [/not reached the scheduling Node/i, '消息还没送到负责这个对话的设备，送到后会自动排队。'],
  [/conversation has no scheduling Node|does not schedule the conversation/i, '这个对话不由这台设备负责。'],
  [/conversation not found/i, '这台设备上还没有这个对话，可能还在同步。'],
  [/work not found|no work was dispatched/i, '找不到这个任务，可能还在同步。'],
  [/pending check not found|only unverified work/i, '这项确认已处理或不存在。'],
  [/scheduler inbox is full/i, '排队的消息太多，这条没有收下。请稍后再发。'],
  [/attachment|blob/i, '附件还没同步到这台设备，或校验没通过。'],
  [/artifact/i, '这个产物现在读不了。'],
  [/outside (the execution Node's )?configured roots/i, '这个文件夹不在这台设备允许的文件夹里。去设备页添加。'],
  [/set a webhook (URL|secret) first/i, '先填好 Webhook 地址和密钥。'],
  [/webhook must use HTTPS/i, 'Webhook 地址要以 https 开头。'],
  [/nothing to change|is unchanged/i, '内容没有变化。'],
  [/project root|execution path|workspace must be|workspace does not exist|manager workspace/i, '文件夹不存在，或不是这台设备上的绝对路径。'],
  [/coordination Agent|manager provider|manager program/i, '这台设备上的 Bot 还没配置好。'],
  [/only the user|requires the user|agents cannot|Agent tokens/i, '只有你自己能做这个操作，Bot 不能代办。'],
  [/management client|management permission|management control/i, '只有管理设备能做这个操作。'],
  [/local browser (session|cookie|access)|local (cookie|token) missing|session cookie|local session identity/i, '本机会话已失效。请在这台电脑上重新打开。'],
  [/ticket has already been used|ticket is unknown or already expired|ticket has expired/i, '这条加入链接已用过或已过期。请在已有设备上点“添加设备”重新生成。'],
  [/invite link|ticket/i, '加入链接无效或不完整。请重新复制。'],
  [/already belongs to a personal network|already initialized/i, '这台设备已经加入了一组设备。'],
  [/revoked/i, '这台设备已被移除。'],
  [/recovery (key|proof)/i, '恢复文件和这组设备对不上，或恢复授权已过期。'],
  [/signature|proof|handshake|identity/i, '设备身份校验没通过，已断开。'],
  [/too large|too long|exceeds|limit/i, '内容超过了大小限制。'],
  [/invalid|is required|must be|missing/i, '请求内容不完整或格式不对。请刷新页面后重试。'],
]

const chinese = /[一-鿿]/

/** Chinese stays as written; known backend text and codes are translated; the rest names the failed action. */
export function humanError(raw: string, code?: unknown, action = '这次操作'): string {
  if (typeof code === 'string' && codes[code]) return codes[code]
  if (chinese.test(raw)) return raw
  return patterns.find(([pattern]) => pattern.test(raw))?.[1] ?? `${action}没有成功，请再试一次。`
}

/** The untranslated text, for 详细信息; empty when the shown message already is the original. */
export function rawError(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error ?? '')
  return chinese.test(raw) ? '' : raw
}
