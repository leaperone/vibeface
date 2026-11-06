import type { LauncherStatus } from '../shared/contract.js'

/**
 * The launcher's whole state machine, kept free of the DOM so it can be tested directly.
 * Every line it produces comes from something the main process actually observed: there is
 * no optimistic wording and no progress that is not a real attempt count.
 */
export type Tone = 'idle' | 'live' | 'busy' | 'bad'

export type ActionId = 'choose' | 'start' | 'attach' | 'stop' | 'open' | 'recheck'

export type Action = {
  id: ActionId
  label: string
  primary: boolean
  disabled: boolean
}

export type View = {
  tone: Tone
  indicator: string
  headline: string
  explain: string
  facts: Array<[string, string]>
  failure: string | null
  actions: Action[]
  scope: string
}

const action = (id: ActionId, label: string, primary = false, disabled = false): Action => ({
  id,
  label,
  primary,
  disabled,
})

const choose = (label = '选择数据目录…', primary = false) => action('choose', label, primary)
const recheck = () => action('recheck', '重新检查')

export function describe(status: LauncherStatus): View {
  const scope = status.attachOnly
    ? '仅连接模式：这次运行不会启动或停止任何 Core。'
    : '管理这台电脑上的 Core，并打开它提供的界面。'

  if (status.runtime.kind === 'missing') {
    return {
      tone: 'bad',
      indicator: '运行时缺失',
      headline: '找不到捆绑的 Core 程序',
      explain: status.runtime.detail,
      facts: [['期望路径', status.runtime.corePath]],
      failure: null,
      actions: [],
      scope,
    }
  }

  const core = status.core
  switch (core.kind) {
    case 'none':
      return {
        tone: 'idle',
        indicator: '未选择目录',
        headline: '选择 Core 数据目录',
        explain:
          '数据目录保存这台设备的身份、配置和本机记录。可以选择已有目录连接正在运行的 Core，也可以选择一个空目录由 Vibeface 初始化。',
        facts: [],
        failure: null,
        actions: [choose('选择数据目录…', true)],
        scope,
      }

    case 'uninitialized':
      return {
        tone: 'idle',
        indicator: '尚未初始化',
        headline: '这个目录还没有 Core 数据',
        explain: status.attachOnly
          ? '仅连接模式不会初始化目录。请选择一个已经在运行 Core 的数据目录。'
          : '启动时会在这里创建身份与配置，默认网页入口 127.0.0.1:4790，本机会话入口 127.0.0.1:4791。目录中已有的文件不会被改动。',
        facts: [['数据目录', core.dataDir]],
        failure: null,
        actions: status.attachOnly
          ? [choose('换一个目录'), action('attach', '仅连接已运行的 Core')]
          : [action('start', '初始化并启动', true), choose('换一个目录')],
        scope,
      }

    case 'offline':
      return {
        tone: 'idle',
        indicator: '未运行',
        headline: 'Core 未在运行',
        explain: status.attachOnly
          ? '仅连接模式不会启动 Core。等这台电脑上的服务启动后再点重新检查。'
          : '可以由 Vibeface 启动并托管这个 Core；退出 Vibeface 时它会一起正常停止。也可以在别处启动服务后仅连接。',
        facts: [
          ['数据目录', core.dataDir],
          ...(core.deviceId ? ([['设备 ID', core.deviceId]] as Array<[string, string]>) : []),
        ],
        failure: null,
        actions: status.attachOnly
          ? [action('attach', '仅连接已运行的 Core', true), recheck(), choose('换一个目录')]
          : [action('start', '启动本机 Core', true), action('attach', '仅连接已运行的 Core'), choose('换一个目录')],
        scope,
      }

    case 'starting':
      return {
        tone: 'busy',
        indicator: '正在启动',
        headline: '正在启动本机 Core',
        explain:
          core.attempts > 0
            ? `已经向 Core 确认 ${core.attempts} 次运行状态，等它报告就绪后会自动打开界面。`
            : '已经启动子进程，正在等待它报告运行状态。',
        facts: [['数据目录', core.dataDir]],
        failure: null,
        actions: [action('open', '打开 Vibeface 窗口', true, true), action('stop', '停止本机 Core', false, true)],
        scope,
      }

    case 'owned':
      return {
        tone: 'live',
        indicator: '本机托管',
        headline: 'Core 正在由 Vibeface 运行',
        explain: '退出 Vibeface 会一并正常停止这个 Core。关闭窗口只会收起界面，进程继续运行。',
        facts: [
          ['数据目录', core.dataDir],
          ['设备 ID', core.deviceId],
          ['本机入口', core.origin],
        ],
        failure: null,
        actions: [action('open', '打开 Vibeface 窗口', true), action('stop', '停止本机 Core'), recheck()],
        scope,
      }

    case 'attached':
      return {
        tone: 'live',
        indicator: '已连接',
        headline: '已连接到这台电脑上的 Core',
        explain: '这个 Core 不是由 Vibeface 启动的，退出 Vibeface 不会影响它。',
        facts: [
          ['数据目录', core.dataDir],
          ['设备 ID', core.deviceId],
          ['本机入口', core.origin],
        ],
        failure: null,
        actions: [action('open', '打开 Vibeface 窗口', true), recheck(), choose('换一个目录')],
        scope,
      }

    case 'failed':
      return {
        tone: 'bad',
        indicator: '出错',
        headline: '上一步没有完成',
        explain: '下面是 Core 返回的原因。数据目录和已有服务都没有被改动。',
        facts: core.dataDir ? [['数据目录', core.dataDir]] : [],
        failure: core.message,
        actions: [recheck(), choose('换一个目录')],
        scope,
      }
  }
}
