/** Join, recovery and device copy the web and the phone show word for word. */
export const joinCopy = {
  waiting: '等待批准',
  showCode: '在发链接的设备上点“显示安全码”',
  compareCode: '两边的安全码一致，就在那边点允许',
  linkSource: (from = '已有设备') => `链接来自${from}上的“添加设备”`,
  byLink: '用加入链接',
}

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
  needsPassword: '没有恢复密码，这个文件打不开',
  onlyCopy: '只有这一份，丢了不能补',
  keptApart: '已把文件和密码分开保存',
}

export const deviceCopy = {
  offline: '确认它开着，并且已联网',
  removeNotice: (bot: boolean) => ['移除后它无法再连接，要再用需重新加入', bot ? '上面的 Bot 会停止' : null, '对话记录会保留'].filter(Boolean).join('，'),
}
