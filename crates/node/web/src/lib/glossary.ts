import {
  Ban, Bot, CircleAlert, CirclePause, CircleQuestionMark, Clock, Crown, FileKey, FolderOpen, Globe, Hand, Inbox,
  Laptop, Link, ListChecks, Lock, Monitor, MonitorSmartphone, QrCode, RefreshCw, ScrollText, Server, Settings, ShieldCheck,
  Smartphone, Sparkles, Workflow, ArchiveRestore,
} from "lucide-react"

/** One icon and one word per concept, see docs/design/web-ux.md section 2. */
export const terms = {
  myDevices: { label: "我的设备", icon: MonitorSmartphone },
  device: { label: "设备", icon: Server },
  thisComputer: { label: "这台电脑", icon: Laptop },
  browser: { label: "浏览器", icon: Monitor },
  phone: { label: "手机", icon: Smartphone },
  address: { label: "访问地址", icon: Globe },
  canRunAssistant: { label: "能运行助手", icon: Bot },
  encrypted: { label: "已加密", icon: ShieldCheck },
  joinLink: { label: "加入链接", icon: Link },
  qr: { label: "二维码", icon: QrCode },
  safetyCode: { label: "安全码", icon: ShieldCheck },
  admin: { label: "管理员", icon: Crown },
  remove: { label: "移除设备", icon: Ban },
  recoveryFile: { label: "恢复文件", icon: FileKey },
  recoveryPassword: { label: "恢复密码", icon: Lock },
  restore: { label: "用恢复文件找回", icon: ArchiveRestore },
  assistant: { label: "助手", icon: Bot },
  aiService: { label: "AI 服务", icon: Sparkles },
  task: { label: "任务", icon: ListChecks },
  approval: { label: "需要你同意", icon: Hand },
  confirm: { label: "需要确认", icon: CircleQuestionMark },
  inbox: { label: "待处理", icon: Inbox },
  folders: { label: "可用文件夹", icon: FolderOpen },
  readOnly: { label: "只读", icon: Lock },
  syncing: { label: "同步中", icon: RefreshCw },
  sending: { label: "发送中", icon: Clock },
  undelivered: { label: "未送达", icon: CircleAlert },
  settings: { label: "设置", icon: Settings },
} as const

export const autonomyTerms = {
  off: { label: "停下", icon: CirclePause },
  report: { label: "只汇报", icon: ScrollText },
  continue: { label: "继续做", icon: Workflow },
} as const
