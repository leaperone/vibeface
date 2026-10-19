/**
 * The one place backend and protocol errors become words a person reads (docs/design/web-ux.md 4.7).
 * Nodes answer in English; the page never shows that text as the message, only under 详细信息.
 */
const codes: Record<string, string> = {
  target_not_member: '这台设备不在我的设备里，访问地址不会转发给它。',
  target_unreachable: '访问地址连不上这台设备。它可能关机或断网了，稍后会自动重试。',
}

const patterns: [RegExp, string][] = [
  [/outcome may be unknown|timed out/i, '设备没有及时回复，结果未知。请先确认结果，不要重复提交。'],
  [/network has not started|maintenance lease|another maintenance/i, '这台设备正在启动或维护，请稍后再试。'],
  [/another personal network|wrong personal network|wrong membership network|unknown network|network mismatch/i, '这台设备属于另一组设备。'],
  [/not a member of the personal network|client identity missing/i, '这个浏览器还不在我的设备里。'],
  [/write needs the signature|signed by another device|client_id differs/i, '这个浏览器的身份校验没通过。请刷新页面后重试。'],
  [/autonomy changed/i, '后续授权已在别处更改。请刷新确认后再改。'],
  [/management reply changed/i, '回复状态已变化，请刷新后再停止。'],
  [/scheduled on another device/i, '这条回复由另一台设备负责，请到那台设备上停止。'],
  [/answered on the executing device/i, '这个请求要在执行任务的设备上回答。'],
  [/already answered or expired|executor restarted; this request expired/i, '这个请求已经回答过或已过期。'],
  [/not waiting for an answer/i, '这个任务现在不需要回答。'],
  [/answer every question|answers? (is|are) (missing|required)|answers must be text/i, '请回答每个问题后再提交。'],
  [/message ID reused|work ID reused|conversation ID already exists/i, '这个编号已用于不同的内容，没有重复提交。'],
  [/not reached the scheduling Node/i, '消息还没送到负责这个对话的设备，送到后会自动排队。'],
  [/conversation has no scheduling Node|does not schedule the conversation/i, '这个对话不由这台设备负责。'],
  [/conversation not found/i, '这台设备上还没有这个对话，可能还在同步。'],
  [/work not found|no work was dispatched/i, '找不到这个任务，可能还在同步。'],
  [/pending check not found|only unverified work/i, '这项确认已处理或不存在。'],
  [/evidence must be|evidence is required/i, '说明需要 12 到 4096 字节。'],
  [/scheduler inbox is full/i, '排队的消息太多，这条没有收下。请稍后再发。'],
  [/attachment|blob/i, '附件还没同步到这台设备，或校验没通过。'],
  [/artifact/i, '这个产物现在读不了。'],
  [/project root|execution path|workspace must be|workspace does not exist|manager workspace/i, '文件夹不存在，或不是这台设备上的绝对路径。'],
  [/coordination Agent|manager provider|manager program/i, '这台设备上的助手还没配置好。'],
  [/only the user|requires the user|agents cannot|Agent tokens/i, '只有你自己能做这个操作，助手不能代办。'],
  [/management client|management permission|management control/i, '只有管理设备能做这个操作。'],
  [/local browser (session|cookie|access)|local (cookie|token) missing|session cookie|local session identity/i, '本机会话已失效。请在这台电脑上重新打开。'],
  [/invite link|ticket/i, '加入链接无效或不完整。请重新复制。'],
  [/already belongs to a personal network|already initialized/i, '这台设备已经加入了一组设备。'],
  [/revoked/i, '这台设备已被移除。'],
  [/recovery (key|proof)/i, '恢复文件和这组设备对不上，或恢复授权已过期。'],
  [/signature|proof|handshake|identity/i, '设备身份校验没通过，已断开。'],
  [/too large|too long|exceeds|limit/i, '内容超过了大小限制。'],
  [/invalid|is required|must be|missing/i, '请求内容不完整或格式不对。请刷新页面后重试。'],
]

const chinese = /[一-鿿]/

/** Chinese stays as written; known backend text and codes are translated; the rest gets a plain sentence. */
export function humanError(raw: string, code?: unknown): string {
  if (typeof code === 'string' && codes[code]) return codes[code]
  if (chinese.test(raw)) return raw
  return patterns.find(([pattern]) => pattern.test(raw))?.[1] ?? '设备没有完成这个操作。'
}

/** The untranslated text, for 详细信息; empty when the shown message already is the original. */
export function rawError(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error ?? '')
  return chinese.test(raw) ? '' : raw
}
