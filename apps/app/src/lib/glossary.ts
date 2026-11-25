import {
  AppWindow, Ban, Bot, CircleAlert, CirclePause, CircleQuestionMark, Crown, FolderOpen, Globe, Hand, Inbox,
  Key, Laptop, ListChecks, Lock, MessageSquareShare, MonitorSmartphone, RefreshCw, ScanEye, Settings, ShieldCheck,
  Sparkles, Star,
} from "lucide-react"

/** One icon and one word per concept, see packages/docs/design/web-ux.md. */
export const terms = {
  myDevices: { label: "我的设备", icon: MonitorSmartphone },
  thisComputer: { label: "这个节点", icon: Laptop },
  browser: { label: "浏览器", icon: AppWindow },
  address: { label: "访问地址", icon: Globe },
  canRunBot: { label: "能运行 Bot", icon: Bot },
  encrypted: { label: "已加密", icon: ShieldCheck },
  safetyCode: { label: "安全码", icon: ScanEye },
  admin: { label: "管理设备", tag: "管理", icon: Crown },
  remove: { label: "移除设备", icon: Ban },
  secret: { label: "密钥", icon: Key },
  bot: { label: "Bot", icon: Bot },
  primaryBot: { label: "主 Bot", icon: Star },
  ai: { label: "Bot 与任务", icon: Sparkles },
  task: { label: "任务", icon: ListChecks },
  approval: { label: "需要你同意", icon: Hand },
  confirm: { label: "待确认", icon: CircleQuestionMark },
  inbox: { label: "待处理", icon: Inbox },
  externalChat: { label: "外部聊天", icon: MessageSquareShare },
  folders: { label: "可用文件夹", icon: FolderOpen },
  readOnly: { label: "只读", icon: Lock },
  paused: { label: "已暂停", icon: CirclePause },
  syncing: { label: "同步中", icon: RefreshCw },
  undelivered: { label: "未送达", icon: CircleAlert },
  settings: { label: "设置", icon: Settings },
} as const
