export const PUBLIC_DOCS = [
  { file: 'overview.md', slug: 'overview', title: 'Vibeface 能帮你做什么', description: '在长期 Bot 聊天里协调自己的电脑，复用已有 Agent，并查看共享任务历史。' },
  { file: 'comparison.md', slug: 'comparison', title: '已有 Agent，为什么还用 Vibeface', description: '按工作方式比较 Codex、Claude Code、OpenClaw 与 Vibeface，选择需要的协调方式。' },
  { file: 'install.md', slug: 'install', title: '安装与管理节点', description: '在自己的 Mac 或 Linux 设备上安装 Vibeface Nightly，并初始化本机 Node。' },
  { file: 'bot-and-execution.md', slug: 'bot-and-execution', title: 'Bot 如何协调，Agent 在哪里执行', description: '分清共享 Bot、协调角色与本机执行，了解复用 Codex 和 Claude Code 的条件。' },
  { file: 'history-sync.md', slug: 'history-sync', title: '换设备时，哪些历史会同步', description: '了解签名 DAG 保存的 Bot 历史、接手条件，以及同步和备份的区别。' },
  { file: 'privacy-security.md', slug: 'privacy-security', title: '谁能读到内容，权限在哪里生效', description: '了解设备身份、传输加密、模型凭证、宿主权限，以及撤销和恢复的边界。' },
  { file: 'node.md', slug: 'node', title: '部署与使用 Node', description: '管理自己的设备、个人网络和本机 Agent。' },
  { file: 'deployment.md', slug: 'deployment', title: '配置并部署 Node', description: '配置数据目录、网络边界与公共入口。' },
  { file: 'troubleshooting.md', slug: 'troubleshooting', title: '排障', description: '从日志、状态和连接信息定位 Vibeface 常见问题。' },
  { file: 'releasing.md', slug: 'releasing', title: '构建与发布', description: '了解 Vibeface 的 CI、制品和 Nightly 发布流程。' },
  { file: 'ci.md', slug: 'ci', title: '按改动选择 CI', description: '查看 Vibeface CI 如何按改动范围选择检查。' },
] as const

export const PUBLIC_DOC_BY_SLUG = new Map<string, (typeof PUBLIC_DOCS)[number]>(PUBLIC_DOCS.map((doc) => [doc.slug, doc]))
