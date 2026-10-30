export const PUBLIC_DOCS = [
  { file: 'install.md', slug: 'install', title: '安装与管理节点', description: '在自己的 Mac 或 Linux 设备上安装 Vibeface Nightly，并初始化本机 Node。' },
  { file: 'node.md', slug: 'node', title: '部署与使用 Node', description: '管理自己的设备、个人网络和本机 Agent。' },
  { file: 'deployment.md', slug: 'deployment', title: '配置并部署 Node', description: '配置数据目录、网络边界与公共入口。' },
  { file: 'troubleshooting.md', slug: 'troubleshooting', title: '排障', description: '从日志、状态和连接信息定位 Vibeface 常见问题。' },
  { file: 'releasing.md', slug: 'releasing', title: '构建与发布', description: '了解 Vibeface 的 CI、制品和 Nightly 发布流程。' },
  { file: 'ci.md', slug: 'ci', title: '按改动选择 CI', description: '查看 Vibeface CI 如何按改动范围选择检查。' },
] as const

export const PUBLIC_DOC_BY_SLUG = new Map<string, (typeof PUBLIC_DOCS)[number]>(PUBLIC_DOCS.map((doc) => [doc.slug, doc]))
