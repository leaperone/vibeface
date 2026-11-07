import type { LauncherStatus, LocalNode } from '../shared/contract.js'

export type Tone = 'idle' | 'live' | 'busy' | 'bad'
export type ActionId =
  | 'controller' | 'local' | 'discover' | 'connect' | 'start-discovered'
  | 'choose' | 'start-owned' | 'stop-owned' | 'open'
  | 'install-background' | 'start-background' | 'stop-background' | 'remove-background'
export type Action = { id: ActionId; label: string; primary?: boolean; disabled?: boolean; nodeId?: string }
export type NodeRow = { node: LocalNode; status: string; detail: string; actions: Action[] }
export type View = {
  tone: Tone
  indicator: string
  headline: string
  explain: string
  facts: Array<[string, string]>
  failure: string | null
  actions: Action[]
  rows: NodeRow[]
  discoveryMessage: string
  backgroundMessage: string
  backgroundActions: Action[]
  scope: string
}

const action = (id: ActionId, label: string, primary = false, disabled = false, nodeId?: string): Action => ({ id, label, primary, disabled, ...(nodeId ? { nodeId } : {}) })

const unavailable = (node: Extract<LocalNode, { kind: 'unavailable' }>): string => ({
  identity_mismatch: '身份信息不一致，请重新检查。',
  maintenance_unavailable: '节点正在维护，稍后重新检查。',
  inspection_unavailable: '暂时无法读取节点状态，请重新检查。',
})[node.reason]

function rows(status: LauncherStatus): NodeRow[] {
  if (status.discovery.kind !== 'ready') return []
  return status.discovery.nodes.map((node) => {
    if (node.kind === 'running') return { node, status: '正在运行', detail: `节点 ${node.deviceId.slice(0, 12)} · ${node.coreVersion}`, actions: [action('connect', '连接', true, false, node.id)] }
    if (node.kind === 'stopped') return { node, status: '已停止', detail: `节点 ${node.deviceId.slice(0, 12)} · ${node.coreVersion}`, actions: [action('start-discovered', '启动并连接', true, false, node.id)] }
    return { node, status: '暂不可用', detail: unavailable(node), actions: [action('discover', '重新检查')] }
  })
}

export function describe(status: LauncherStatus): View {
  const discoveryMessage = status.discovery.kind === 'scanning'
    ? '正在检查已知的本机节点…'
    : status.discovery.kind === 'failed'
      ? status.discovery.message
      : status.discovery.kind === 'ready'
        ? status.discovery.warnings.length ? `有 ${status.discovery.warnings.length} 项位置未能检查。` : ''
        : ''

  const background = status.backgroundNode
  const backgroundMessage = background.kind === 'installed'
    ? `后台节点${background.health === 'running' ? '正在运行' : background.health === 'stopped' ? '已停止' : '状态异常'} · ${background.nodeId.slice(0, 12)} · ${background.dataDir}`
    : background.kind === 'checking' ? '正在核对后台节点状态…'
      : background.kind === 'partial' || background.kind === 'conflict' || background.kind === 'failure' ? background.message
        : '安装后台节点后，退出 Vibeface 仍会运行，并在此用户登录后启动。'
  const backgroundActions: Action[] = background.kind === 'absent'
    ? [action('install-background', '安装后台节点')]
    : background.kind === 'installed'
      ? [
          ...(background.health === 'running' ? [action('stop-background', '停止后台节点')] : [action('start-background', '启动后台节点')]),
          action('remove-background', '移除后台运行'),
        ]
      : []

  if (status.nodeMode === 'undecided') return {
    tone: 'idle', indicator: '可开始使用', headline: 'Vibeface',
    explain: '你可以连接个人网络中的节点，也可以选择在这台电脑启用节点。',
    failure: null,
    facts: [],
    actions: [action('controller', '仅作控制端', true), action('local', '在这台电脑启用节点')],
    rows: [], discoveryMessage: '', backgroundMessage: '', backgroundActions: [],
    scope: '桌面端可独立连接和控制节点。',
  }

  const isLocal = status.nodeMode === 'local'
  const hasCore = status.core.kind === 'owned' || status.core.kind === 'attached'
  const failed = status.discovery.kind === 'failed'
  const actions: Action[] = [
    ...(status.clientAvailable ? [action('open', hasCore ? '打开 Vibeface' : '连接节点', true)] : []),
    action('discover', '重新检查本机节点'),
    ...(isLocal && !hasCore ? [action('start-owned', '在此电脑新建节点'), action('choose', '选择已有数据目录…')] : []),
    ...(status.core.kind === 'owned' ? [action('stop-owned', '停止随 App 运行的节点')] : []),
  ]
  const core = status.core
  const failure = core.kind === 'failed' ? core.message : failed ? status.discovery.kind === 'failed' ? status.discovery.message : null : null
  const tone: Tone = failure ? 'bad' : hasCore ? 'live' : status.discovery.kind === 'scanning' ? 'busy' : 'idle'
  const headline = hasCore ? '节点已连接' : status.nodeMode === 'controller' ? '连接你的 Vibeface 节点' : '在这台电脑启用节点'
  const explain = hasCore
    ? '控制端连接与本机节点运行彼此独立；切换连接目标不会停止本机节点。'
    : status.nodeMode === 'controller'
      ? '选择一个正在运行的节点即可开始。已停止的节点只会在你明确选择后启动。'
      : '可以连接已有节点，或在此电脑创建新节点。退出 App 会停止由 App 临时运行的节点。'
  return {
    tone, indicator: hasCore ? '已连接' : failed ? '检查未完成' : '等待连接', headline, explain,
    facts: core.kind === 'owned' || core.kind === 'attached' ? [['节点 ID', core.deviceId], ['数据目录', core.dataDir]] : [],
    failure, actions, rows: rows(status), discoveryMessage,
    backgroundMessage: isLocal || background.kind !== 'absent' ? backgroundMessage : '',
    backgroundActions: isLocal ? backgroundActions : [],
    scope: isLocal ? '这台电脑可以运行节点，也可以连接其他节点。' : '此模式只控制节点，不会在这台电脑启用节点。',
  }
}
