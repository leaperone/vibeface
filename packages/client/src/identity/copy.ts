import type { Member, PendingRecovery } from '../lib/node.ts'

/** Join, recovery and device copy the web and the phone show word for word. */
export const joinCopy = {
  waiting: '等待批准',
  showCode: '正在生成安全码…',
  compareCode: '两边的安全码一致，就在那边点允许',
  linkSource: (from = '已有设备') => `链接来自${from}上的“添加设备”`,
  byLink: '用加入链接',
  scan: '扫码加入',
  paste: '或粘贴加入链接',
  pasteFirst: '粘贴加入链接',
  linkFilled: '已填入加入链接',
  notLink: '这不是有效的加入链接',
  deviceName: '这台设备的名字',
  join: '加入',
  joining: '加入中',
  noComputer: '还没有电脑？',
  installFirst: '先在电脑上安装 Vibeface',
  installGuide: 'https://github.com/leaperone/vibeface/blob/main/docs/install.md',
}

/**
 * Why this computer's join ended and what to do next, from the `reason` the Node reports. The patterns and words follow
 * `join_failure_text` in crates/node/src/main.rs, which `vibeface join` prints; only the next step is the page's own.
 */
export const nodeJoinCopy = {
  used: '这条加入链接已经用过了。在管理设备上点“添加设备”重新生成，再粘贴新链接。',
  expired: '这条加入链接已过期。在管理设备上点“添加设备”重新生成，再粘贴新链接。',
  failed: '没能加入。检查网络后再点“加入”，会接着上次的请求，不会重复提交。',
  /** The page lost the computer's answer; the join may still be running there. */
  unreachable: '连不上这台电脑上的 Vibeface。加入可能还在进行：确认它在运行后再点“加入”，会接着上次的请求，不会重复提交。',
}
const nodeJoinReasons: [string, string][] = [
  ['already been used', nodeJoinCopy.used],
  ['expired', nodeJoinCopy.expired],
  ['Rejected', '管理设备拒绝了这次加入。确认是自己的电脑后，在管理设备上重新生成链接再试。'],
  ['Withdrawn', '这次加入已撤回。在管理设备上重新生成链接再试。'],
  ['approval timed out', '等了很久也没有批准。在管理设备上重新生成链接再试。'],
  ['already belongs to a personal network', '这台电脑已经在一个网络里了，不用再加入。'],
  ['another join is already running', '这台电脑正在用终端命令加入。等它结束后再试。'],
  ['another join is in progress', '这条链接上次用的是另一个名字。改回上次的名字再加入，会接着等批准。'],
  ['invite link', '这不是有效的加入链接。请从管理设备的“添加设备”重新复制。'],
  ['Vibeface ticket', '这不是有效的加入链接。请从管理设备的“添加设备”重新复制。'],
]
export const nodeJoinFailure = (reason: string) => nodeJoinReasons.find(([pattern]) => reason.includes(pattern))?.[1] ?? nodeJoinCopy.failed

/** A computer's own page before it belongs anywhere: two equal ways on, so a second computer does not start a second network. */
export const startCopy = {
  create: '新建我的网络',
  createNote: '第一次用 Vibeface',
  join: '加入已有网络',
  joinNote: '有加入链接',
}

/** The computer this device reaches its network through does not answer; `name`: that computer as the person named it. */
export const offlineCopy = {
  title: (name: string) => `连不上${name || '你的电脑'}`,
  hint: '确认它开着',
  retry: '重试',
  retryConnect: '重试连接',
}

/** 手动填写地址, under 详细信息: a device ID pins which computer answers there. */
export const manualCopy = {
  optional: '这台电脑可不填',
  where: '在我的设备 → 那台设备 → 详细信息里复制',
}

/**
 * 添加设备: a phone scans the code. Another computer opens the link on its own Vibeface page, or runs one command there,
 * running or not; both ways show without opening the raw link.
 */
export const addDeviceCopy = {
  phone: '手机',
  computer: '电脑',
  copyCommand: '复制命令',
  openThere: '在那台电脑的 Vibeface 网页选“加入已有网络”，粘贴',
  installThere: '那台电脑要先装好 Vibeface',
  installHow: '怎么装',
  orCommand: '或在那台电脑的终端运行',
  /** The browser could not write to the clipboard; the text below opens selected instead. */
  copyFailed: '没能复制，请手动选中下面的内容',
  compare: '新设备出现安全码后，回这里比对',
  /** The Node numbers a name another device already has, so one suggested name is enough. */
  command: (link: string) => `vibeface join '${link}' --name '我的电脑'`,
}
/** Where a device without management goes to add one; `managers`: the names of the management devices. */
export const addOnManager = (managers: readonly string[]) => managers.length === 1 ? `在管理设备「${managers[0]}」上添加` : '在管理设备上添加'

/** Settings' two red rows each say what they cost before either is tapped. */
export const exitCopy = { leave: '要用加入链接才能回来', reset: '清除本机数据，回到欢迎页' }

/** The only management device is a browser, whose key goes with its site data; `name`: that browser, 这个浏览器 when it is this one. */
export const soleBrowserManagerText = (name: string) => `清除${name}的数据会失去管理权`
export const anotherManager = '再设一台管理设备'

/** Leaving needs another management device first; `self` names this one (这台手机, 这个浏览器). */
export const onlyManagerText = (self: string) => `${self}是唯一的管理设备，先把另一台设为管理设备再断开`
/** An erase takes this device's management with it; the phone warns before clearing itself. */
export const eraseSoleManagerText = (self: string) => `${self}是唯一的管理设备，清除后这个网络就没有管理设备了。`
/** A join link from another network, opened on a device already in one. */
export const switchCopy = {
  title: '换到新的网络？',
  erase: (self: string) => `${self}会离开当前网络，并清除本机数据`,
  leave: (self: string) => `${self}会离开当前网络`,
  confirm: '换网络',
}
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
  byPhone: '在手机上完成找回',
  qr: '找回二维码',
  scanHint: '在手机 App 的“用恢复文件找回”里扫码',
  unreachable: '手机连不到这台电脑，先连同一个 Wi-Fi 再看',
  scan: '扫电脑上的找回二维码',
  paste: '或粘贴找回链接',
  pasteLabel: '找回链接',
  notRecoveryLink: '这不是找回链接，到电脑网页的设置 → 找回 → 在手机上完成找回里复制',
  target: '要连的电脑',
  replaceHint: '要用现在的恢复文件和它的密码，换后旧文件作废',
  replaced: '已换密码，旧恢复文件已失效',
  waiting: '找回等待生效',
  left: (at: number, now = Date.now()) => at > now ? `剩余 ${hoursUntil(at, now)} 小时` : '马上生效',
  autoEnter: '生效后自动进入',
  managersSee: '你的管理设备会看到提醒',
  stopped: '这次找回被拦下了',
  stoppedNote: '新恢复文件用不了，旧文件仍然有效',
  saveNew: '保存新的恢复文件',
  recovered: '已找回，旧文件已作废',
  why: '设备都丢了时，用它和恢复密码找回',
  passwordWhy: '设备都丢了时，用它和恢复文件找回',
  onlyCopy: '只能在这一步下载，之后补不回',
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
  file: '恢复文件',
  codeLabel: '识别码',
  day: (at: number) => new Date(at).toLocaleDateString('zh-CN'),
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
  /** 可用文件夹: what it is, and during setup that it can wait. */
  folders: (later: boolean) => later ? 'Bot 能读写的项目文件夹，可稍后添加' : 'Bot 能读写的项目文件夹',
  /** A computer's bot as `botReadiness` reports it; `null`: it has none set up. */
  botStatus: { ready: '能运行 Bot', starting: 'Bot 启动中', needs_setup: 'Bot 需设置', none: '还没配置 Bot', unknown: '离线，状态未知' },
  /** An offline computer reports nothing, so no bot there is unknown rather than none. */
  botState: (bot: 'ready' | 'starting' | 'needs_setup' | null | undefined, online: boolean | undefined) => bot ?? (online === false ? 'unknown' as const : 'none' as const),
  /** This computer has a provider installed but no use of any turned on, as a computer that just joined starts. */
  aiOff: '这台电脑的 AI 还没开',
  removeNotice: (bot: boolean) => ['移除后它无法再连接，要再用需重新加入', bot ? '上面的 Bot 会停止' : null, '对话记录会保留'].filter(Boolean).join('，'),
  /**
   * A provider's two uses on a device: one provider runs the bot, any number execute tasks. `note` is the line shown
   * above the providers; `hint` says it again for one provider's control.
   */
  roles: {
    bot: { label: '运行 Bot', note: '只选一个，Bot 用它回复聊天', hint: (provider: string) => `Bot 用这台设备的 ${provider} 回复聊天` },
    execution: { label: '执行任务', note: '可开多个，Bot 派的任务交给它做', hint: (provider: string) => `Bot 派的任务用这台设备的 ${provider} 来做` },
  },
  /** How Codex work reaches an App Server: the one already running on that computer, or one Vibeface starts. */
  appServer: { system: '用已在运行的', spawn: '由 Vibeface 启动', useSpawn: '改由 Vibeface 启动' },
  /** The bot's model: a choice from the list its provider gives, with 其他… and 从列表选 switching to and from typing it. */
  botModel: { label: 'Bot 模型', default: '默认', other: '其他…', pick: '从列表选' },
}

/** Settings → 会话等你时: how a session that needs the person reaches them. `outlets`: where it shows by default. */
export const waitingCopy = {
  title: '会话等你时',
  forward: '转发到外部聊天',
  /** One line under the two fields: what can receive it and where the two values come from. */
  forwardHint: '支持能收 HTTPS 消息的外部聊天，如 Grok Bot',
  urlHint: '外部聊天收消息的地址',
  secretHint: '自己设一串，外部聊天那边填同一串',
  forwardHelp: 'https://github.com/leaperone/vibeface/blob/main/docs/node.md#转发到外部聊天',
  outlets: { record: '只在待处理', both: '也发给外部聊天' },
  /** The home tip after joining: one tap asks the system, closing it keeps it closed. */
  notifyTip: '会话等你时提醒你',
  notifyOn: '打开通知',
}
