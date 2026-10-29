import type { Member, PendingRecovery } from '../lib/node.ts'

/** Join, recovery and device copy the web and the phone show word for word. */
export const joinCopy = {
  waiting: '等待批准',
  showCode: '在发链接的设备上点“显示安全码”',
  compareCode: '两边的安全码一致，就在那边点允许',
  linkSource: (from = '已有设备') => `链接来自${from}上的“添加设备”`,
  byLink: '用加入链接',
  linkFilled: '已填入加入链接',
  deviceName: '这台设备的名字',
}

/** Leaving needs another management device first; `self` names this one (这台手机, 这个浏览器). */
export const onlyManagerText = (self: string) => `${self}是唯一的管理设备，先把另一台设为管理设备再断开`
/** The login this device kept only in its own storage is gone; the web and the phone say so the same way. */
export const lostKeyText = (self: string) => `${self}保存的登录信息不见了。用加入链接重新加入，或用恢复文件找回。原任务不会因此重新执行。`

/** Leaving would take the last way to approve devices and settings with it, so both apps explain instead of offering it. */
export const soleManager = (members: readonly Member[], me: string | undefined) =>
  members.some((member) => member.device_id === me && member.manage) && !members.some((member) => member.device_id !== me && member.manage)

/** Hours until `at`, at least one while it has not passed. */
export const hoursUntil = (at: number, now = Date.now()) => Math.max(1, Math.ceil((at - now) / 3_600_000))

/** A time within the next days as people say it: 今天、明天、后天 and the clock, later ones with their date. */
export function whenText(at: number, now = Date.now()) {
  const date = new Date(at), today = new Date(now)
  const day = (value: Date) => Date.UTC(value.getFullYear(), value.getMonth(), value.getDate()) / 86_400_000
  const pad = (value: number) => String(value).padStart(2, '0')
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`
  const ahead = day(date) - day(today)
  return ahead >= 0 && ahead <= 2 ? `${['今天', '明天', '后天'][ahead]} ${clock}` : `${date.getMonth() + 1}月${date.getDate()}日 ${clock}`
}

export const recoveryCopy = {
  scope: '只找回设备和管理权限，不含对话',
  whichLost: '哪台设备丢了？',
  delay: '2 天后生效，期间你的其他设备可以拦下',
  cancelLate: '如果已经提交，取消不会撤回。',
  connectTo: '连接到哪台电脑',
  manual: '手动填写',
  replaceHint: '需要现在的恢复文件和旧密码，旧文件会作废',
  replaced: '已换密码，旧恢复文件已失效',
  waiting: '找回等待生效',
  left: (at: number, now = Date.now()) => at > now ? `剩余 ${hoursUntil(at, now)} 小时` : '马上生效',
  autoEnter: '生效后自动进入',
  managersSee: '你的管理设备会看到提醒',
  stopped: '这次找回被拦下了',
  stoppedNote: '新恢复文件用不了，旧文件仍然有效',
  saveNew: '保存新的恢复文件',
  recovered: '已找回，旧文件已作废',
  why: '设备都丢了时，靠它找回',
  needsPassword: '没有恢复密码，这个文件打不开',
  onlyCopy: '只有这一份，丢了不能补',
  keptApart: '已把文件和密码分开保存',
  pending: '有人正在用恢复文件找回',
  effectiveIn: (at: number, now = Date.now()) => `${hoursUntil(at, now)} 小时后生效`,
  outIfNotStopped: '不拦下，到时划掉的设备会被移出',
  added: '新设备',
  itsMe: '是我在找回',
  stop: '拦下',
  stopAsk: '拦下这次找回？',
  stopNote: '拦下后对方的新恢复文件用不了，设备保持不变',
  stopDone: '已拦下',
  leaked: '恢复文件可能泄露了',
  leakedNote: '对方还能再试，换密码让旧文件作废',
  later: '以后再说',
  rotate: '换恢复密码',
}

/** What a pending recovery would change: the devices it brings in, then the ones here it would remove. */
export function recoveryChanges(pending: PendingRecovery, current: Member[]) {
  const kept = new Set(pending.members.map((member) => member.device_id))
  const known = new Set(current.map((member) => member.device_id))
  return [
    ...pending.members.filter((member) => !known.has(member.device_id)).map((member) => ({ member, added: true })),
    ...current.filter((member) => !kept.has(member.device_id)).map((member) => ({ member, added: false })),
  ]
}

export const deviceCopy = {
  offline: '确认它开着，并且已联网',
  manageHint: '能添加、移除设备，改设置，拦下找回',
  removeNotice: (bot: boolean) => ['移除后它无法再连接，要再用需重新加入', bot ? '上面的 Bot 会停止' : null, '对话记录会保留'].filter(Boolean).join('，'),
  /** A provider's two switches on a device; `hint` says what each turns on, named for the provider's row. */
  roles: {
    bot: { label: '运行 Bot', hint: (provider: string) => `Bot 用这台设备的 ${provider} 回复聊天` },
    execution: { label: '执行任务', hint: (provider: string) => `Bot 派的任务用这台设备的 ${provider} 来做` },
  },
}
