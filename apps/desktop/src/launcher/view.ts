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
    ? '连接已有的 Vibeface，此入口不会管理其运行。'
    : '在这台电脑上使用 Vibeface。'

  if (status.runtime.kind === 'missing') {
    return {
      tone: 'bad',
      indicator: '运行时缺失',
      headline: 'Vibeface 程序不完整',
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
        indicator: '尚未开始',
        headline: '开始使用 Vibeface',
        explain:
          status.attachOnly
            ? '选择已经在运行的 Vibeface 所使用的数据位置。'
            : '开始后，身份和记录会保存在这台电脑的应用目录。已有数据时也可以选择原来的位置。',
        facts: [],
        failure: null,
        actions: status.attachOnly
          ? [choose('选择数据目录…', true)]
          : [action('start', '在这台电脑开始使用', true), choose('使用已有数据…')],
        scope,
      }

    case 'uninitialized':
      return {
        tone: 'idle',
        indicator: '尚未初始化',
        headline: '这个位置还没有配置 Vibeface',
        explain: status.attachOnly
          ? '请选择正在运行的 Vibeface 所使用的数据位置。'
          : '启动会在此位置建立这台电脑的身份与配置，后续设置在 Vibeface 界面完成。',
        facts: [['数据目录', core.dataDir]],
        failure: null,
        actions: status.attachOnly
          ? [choose('换一个目录'), action('attach', '连接已有 Vibeface')]
          : [action('start', '初始化并启动', true), choose('换一个目录')],
        scope,
      }

    case 'offline':
      return {
        tone: 'idle',
        indicator: '未运行',
        headline: 'Vibeface 尚未启动',
        explain: status.attachOnly
          ? '等待这台电脑上的 Vibeface 启动后，再点重新检查。'
          : '在这台电脑上启动后，即可进入 Vibeface 界面。',
        facts: [
          ['数据目录', core.dataDir],
          ...(core.deviceId ? ([['设备 ID', core.deviceId]] as Array<[string, string]>) : []),
        ],
        failure: null,
        actions: status.attachOnly
          ? [action('attach', '连接已有 Vibeface', true), recheck(), choose('换一个目录')]
          : [action('start', '启动 Vibeface', true), action('attach', '连接已有 Vibeface'), choose('换一个目录')],
        scope,
      }

    case 'starting':
      return {
        tone: 'busy',
        indicator: '正在启动',
        headline: '正在启动 Vibeface',
        explain:
          core.attempts > 0
            ? `已检查 ${core.attempts} 次，仍在等待启动完成。`
            : '正在等待这台电脑就绪。',
        facts: [['数据目录', core.dataDir]],
        failure: null,
        actions: [action('open', '打开 Vibeface', true, true), action('stop', '停止在这台电脑运行', false, true)],
        scope,
      }

    case 'owned':
      return {
        tone: 'live',
        indicator: '已启动',
        headline: 'Vibeface 正在这台电脑运行',
        explain: '关闭窗口后会在后台继续运行，选择退出时才会停止。',
        facts: [
          ['数据目录', core.dataDir],
          ['设备 ID', core.deviceId],
          ['本机入口', core.origin],
        ],
        failure: null,
        actions: [action('open', '打开 Vibeface', true), action('stop', '停止在这台电脑运行'), recheck()],
        scope,
      }

    case 'attached':
      return {
        tone: 'live',
        indicator: '已连接',
        headline: '已连接正在运行的 Vibeface',
        explain: '运行不受此窗口控制，退出 App 后也会保持运行。',
        facts: [
          ['数据目录', core.dataDir],
          ['设备 ID', core.deviceId],
          ['本机入口', core.origin],
        ],
        failure: null,
        actions: [action('open', '打开 Vibeface', true), recheck(), choose('换一个目录')],
        scope,
      }

    case 'failed':
      return {
        tone: 'bad',
        indicator: '出错',
        headline: '上一步没有完成',
        explain: '可以重新检查当前状态，或调整设置后重试。',
        facts: core.dataDir ? [['数据目录', core.dataDir]] : [],
        failure: core.message,
        actions: [recheck(), choose('换一个目录')],
        scope,
      }
  }
}
