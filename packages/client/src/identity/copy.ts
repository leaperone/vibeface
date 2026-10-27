/** Join, recovery and device copy the web and the phone show word for word. */
export const joinCopy = {
  waiting: '等待批准',
  showCode: '在发链接的设备上点“显示安全码”',
  compareCode: '两边的安全码一致，就在那边点允许',
  linkSource: (from = '已有设备') => `链接来自${from}上的“添加设备”`,
  byLink: '用加入链接',
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
}

export const deviceCopy = {
  offline: '确认它开着，并且已联网',
  removeNotice: (bot: boolean) => ['移除后它无法再连接，要再用需重新加入', bot ? '上面的 Bot 会停止' : null, '对话记录会保留'].filter(Boolean).join('，'),
}
