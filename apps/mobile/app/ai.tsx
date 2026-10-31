import { Bot, Check, ChevronDown, ChevronRight, CircleAlert, CircleCheck, CircleDashed, List, ListChecks, LoaderCircle, Monitor, Pencil, Play, PlugZap, Plus, Sparkles, Trash2, X, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { deviceCopy } from '@vibeface/client/identity/copy.ts'
import { availability, summaryReadiness, botOnChange, folderSuggestions, type Availability, messageOf, modelTyped, parseAppServerLink, parseNodeSettings, effortOptions, probeIssueText, providerGroups, providerNames, providerIds, type BackupPolicy, type AppServer, type AppServerLink, type AppServerMode, type ManagerSettings, type ModelChoice, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderStatus } from '@vibeface/client/lib/node.ts'
import { folderLabels } from '@vibeface/client/workspace/bot-settings.ts'
import { ErrorText } from '@/components/action'
import { CopyableId, Details, EmptyState, Skeleton, providerIcons, toneText, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { Field, PathList, Retry, useCall } from '@/features/devices/remote-settings'
import { cn } from '@/lib/utils'

/**
 * The AI of the device this phone connects through, as on the web's device page. It goes over the encrypted connection,
 * so an admin phone can set it up remotely. Variable values are write-only.
 */
export default function AiScreen() {
  const { ready } = useDevice()
  if (!ready) return null
  if (!ready.principal.can_manage && ready.principal.source !== 'local_session') return <EmptyState icon={Sparkles} title="只有管理设备能修改" className="flex-1 justify-center" />
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Section icon={Sparkles} title={ready.info.name || 'AI'}><AiSettings /></Section>
  </KeyboardAwareScrollView>
}

const states: Record<Availability, { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: '可用', tone: 'success', icon: CircleCheck },
  unverified: { label: '尚未确认', tone: 'neutral', icon: CircleDashed },
  needs_setup: { label: '需设置', tone: 'warning', icon: CircleAlert },
  starting: { label: '启动中', tone: 'neutral', icon: LoaderCircle },
  missing: { label: '未安装', tone: 'neutral', icon: CircleDashed },
}

const services: Record<string, string> = { CURSOR_API_KEY: 'Cursor', OPENAI_API_KEY: 'OpenAI', ANTHROPIC_API_KEY: 'Anthropic', GEMINI_API_KEY: 'Gemini' }
const keyName = (name: string) => services[name] ? `${services[name]} 密钥` : name

function issueText(provider: ProviderId, issue: ProviderIssue): string {
  const name = providerNames[provider]
  switch (issue.code) {
    case 'bin_missing': return `找不到 ${name} 程序`
    case 'env_missing': return `缺少 ${keyName(issue.name)}`
    case 'launch_failed': return `${name} 没能启动`
    case 'config_invalid': return `${name} 的配置文件读不了`
    case 'app_server_not_found': return '没找到这台电脑上运行的 Codex'
    case 'app_server_unreachable': return '连不上这台电脑上的 Codex'
  }
}

type Change = { bot?: boolean; execution?: boolean; execution_model?: string; bin?: string; env?: Record<string, string | null>; app_server?: { mode: AppServerMode; socket: string | null } }

function AiSettings() {
  const { ready } = useDevice()
  const { value: response, busy, error, call, load, setError } = useCall(parseNodeSettings, 'node_settings')
  const starting = response?.providers.some((item) => item.state === 'starting')
  useEffect(() => {
    if (!starting) return
    const timer = setTimeout(() => void load(), 2000)
    return () => clearTimeout(timer)
  }, [response])
  // Nothing pushes a device's bot readiness or folders; the new bot form and the chats read them from `nodes`, so read them again when they move.
  const live = useLive()
  const botState = response?.providers.find((item) => item.roles.includes('agent'))?.state
  const folders = response?.settings.roots.join('\n')
  useEffect(() => { if (folders !== undefined) live?.refresh().then(() => setError((shown) => shown?.at === 'refresh' ? null : shown), (cause) => { if (ready?.isCurrent()) setError({ at: 'refresh', text: messageOf(cause, '读取最新内容') }) }) }, [botState, folders])
  if (!response) return error ? <Retry error={error.text} onRetry={load} /> : <Skeleton className="h-28 rounded-xl" />
  const { roots, manager } = response.settings
  const test = async (): Promise<AppServerLink | null> => {
    if (!ready?.isCurrent()) return null
    try { const link = parseAppServerLink(await ready.request('test_codex_app_server')); void load(); return link }
    catch (cause) { setError({ at: 'codex', text: messageOf(cause) }); return null }
  }
  const { shown, folded } = providerGroups(response.providers)
  const row = (item: ProviderStatus, index: number) => <ProviderRow key={item.provider} item={item} first={index === 0} response={response} busy={busy} test={test}
    models={item.probe.catalog.state === 'available' ? item.probe.catalog.models : null} probe={async () => { await call(`${item.provider}:probe`, 'provider_probe', { provider: item.provider }, item.provider); if (live?.isCurrent()) await live.refresh() }}
    error={error?.at === item.provider ? error.text : ''}
    change={(key, change) => call(`${item.provider}:${key}`, 'configure_provider', { provider: item.provider, ...change }, item.provider).then(Boolean)}
    saveAssistant={(next) => call(`${item.provider}:assistant_details`, 'configure_node', { roots, manager: next }, item.provider).then(Boolean)} />
  const list = (items: ProviderStatus[]) => <View className="overflow-hidden rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">{items.map(row)}</View>
  return <View className="gap-6">
    <View className="flex-row flex-wrap gap-x-4 gap-y-1 px-1">
      {(['bot', 'execution'] as const).map((role) => <View key={role} accessible accessibilityLabel={`${roles[role].label}，${roles[role].note}`} className="flex-row items-center gap-1.5">
        <Icon as={roles[role].icon} className="size-4 text-muted-foreground" />
        <Text className="text-meta text-muted-foreground"><Text className="text-meta font-medium">{roles[role].label}</Text> · {roles[role].note}</Text>
      </View>)}
    </View>
    {shown.length ? list(shown) : null}
    {folded.length ? <Details label={`未安装 · ${folded.map((item) => providerNames[item.provider]).join('、')}`} open={!shown.length}>{list(folded)}</Details> : null}
    <PathList label="可用文件夹" items={roots} shown={response.display_paths} suggested={folderSuggestions(response)} busy={busy !== ''} onChange={(next) => void call('roots', 'configure_node', { roots: next, manager }, 'roots')} />
    {error && ['roots', 'refresh', ''].includes(error.at) ? <ErrorText error={error.text} /> : null}
  </View>
}

/** One line per provider, as on the web: its uses that are on, and whether it can be used, greyed out without its program. Tapping it opens the switches and settings. */
function ProviderRow({ item, first, response, busy, models, error, change, saveAssistant, test, probe }: {
  item: ProviderStatus; first: boolean; response: NodeSettingsResponse; busy: string; models: ModelChoice[] | null; error: string
  change(key: string, value: Change): Promise<boolean>; saveAssistant(manager: ManagerSettings): Promise<boolean>; test(): Promise<AppServerLink | null>; probe(): Promise<unknown>
}) {
  const name = providerNames[item.provider]
  const assistant = item.roles.includes('agent'), execution = item.roles.includes('execution')
  const manager = assistant ? response.settings.manager : null
  const readiness = summaryReadiness(item)
  const usable = availability(item)
  const state = states[usable]
  const executionState = states[item.execution_readiness.state]
  const issue = item.state === 'needs_setup' ? item.issues[0] : undefined
  const [open, setOpen] = useState(false)
  const [bin, setBin] = useState(item.bin)
  useEffect(() => setBin(item.bin), [item.bin])
  const working = busy.startsWith(`${item.provider}:`)
  const using = [assistant ? roles.bot : null, execution ? roles.execution : null].filter((role) => role !== null)
  return <View className={cn(!first && 'border-t border-border')}>
    <Pressable accessibilityRole="button" accessibilityState={{ expanded: open }} accessibilityLabel={[name, ...using.map((role) => role.label), state.label].join('，')}
      className="min-h-14 flex-row items-center gap-3 px-4 py-2 active:bg-muted/60" onPress={() => setOpen(!open)}>
      <Icon as={providerIcons[item.provider]} className={usable === 'missing' ? 'text-muted-foreground/60' : 'text-muted-foreground'} />
      <Text numberOfLines={1} className={cn('min-w-0 flex-1 font-medium', usable === 'missing' && 'text-muted-foreground')}>{name}</Text>
      {using.map((role) => <Icon key={role.label} as={role.icon} className="size-4 text-muted-foreground" />)}
      <View className="flex-row items-center gap-1">
        <Icon as={state.icon} className={cn('size-4', toneText[state.tone])} />
        <Text className={cn('text-meta', toneText[state.tone])}>{state.label}</Text>
      </View>
      <Icon as={open ? ChevronDown : ChevronRight} className="size-4 text-muted-foreground" />
    </Pressable>
    {issue ? <View className="-mt-1 flex-row flex-wrap items-center gap-x-2 px-4 pb-2">
      <Icon as={CircleAlert} className="size-4 text-warning" />
      <Text className="shrink text-meta text-warning">{issueText(item.provider, issue)}{item.issues.length > 1 ? `，还有 ${item.issues.length - 1} 项` : ''}</Text>
      {issue.code === 'app_server_not_found' && item.app_server
        ? <Button variant="link" className="px-0" disabled={working} onPress={() => void change('app_server', { app_server: { mode: 'spawn', socket: item.app_server?.socket ?? null } })}><Text>{deviceCopy.appServer.useSpawn}</Text></Button>
        : open ? null : <Button variant="link" className="px-0" onPress={() => setOpen(true)}><Text>去设置</Text></Button>}
    </View> : null}
    <View className="gap-1 px-4 pb-3">
      {'issue' in readiness ? <Text className="text-meta text-muted-foreground">{probeIssueText(item.provider, readiness.issue)}</Text> : null}
      {item.setup && (assistant || execution) && ['needs_setup', 'unverified'].includes(readiness.state) ? <View className="gap-2">
        <Text className="text-meta text-muted-foreground">在运行 AI 的电脑上执行以下命令，为独立目录登录或配置，再点“检查可用性”。不会复制已有登录信息。</Text>
        <Text selectable className="font-mono text-meta text-muted-foreground">{item.setup.directory}</Text>
        <CopyableId value={item.setup.command} label={`${name} 私有目录登录命令`} />
      </View> : null}
      <Text className="text-meta text-muted-foreground">{item.probe.checked_at_ms === null ? '尚未主动检查' : `检查于 ${new Date(item.probe.checked_at_ms).toLocaleString()}`}</Text>
      <Button variant="ghost" className="self-start" disabled={busy !== ''} onPress={() => void probe()}><Icon as={busy === `${item.provider}:probe` ? LoaderCircle : PlugZap} /><Text>检查可用性</Text></Button>
      {manager && item.probe.catalog.state === 'available' && item.probe.catalog.coverage === 'partial' ? <Text className="text-meta text-muted-foreground">模型列表可能不完整，可手填其他模型。</Text> : null}
      {manager && item.probe.catalog.state === 'unavailable' ? <Text className="text-meta text-muted-foreground">模型列表暂不可用，可继续手填。已保存的模型保持不变。</Text> : null}
    </View>
    {open ? <>
      {item.supports.includes('agent') ? <Toggle role="bot" name={name} pressed={assistant} busy={busy === `${item.provider}:bot`} disabled={busy !== ''} onPress={() => void change('bot', assistant ? { bot: false } : botOnChange(response, item))} /> : null}
      {item.supports.includes('execution') ? <Toggle role="execution" name={name} pressed={execution} busy={busy === `${item.provider}:execution`} disabled={busy !== ''} onPress={() => void change('execution', { execution: !execution })} /> : null}
      <View className="gap-4 border-t border-border p-4">
        {manager ? <Details label="运行 Bot" keepMounted>
          <AssistantDetails key={`${manager.provider}:${JSON.stringify([manager.model, manager.reasoning_effort, manager.backup])}`} fields={['model']} manager={manager} models={models} working={working} save={saveAssistant} />
        </Details> : null}
        {execution ? <Details label="执行任务" keepMounted>
          <Text className={cn('text-meta', toneText[executionState.tone])}>执行模型：{executionState.label}</Text>
          {'issue' in item.execution_readiness ? <Text className="text-meta text-muted-foreground">{probeIssueText(item.provider, item.execution_readiness.issue)}</Text> : null}
          <ExecutionDetails key={`${item.execution_model}:${item.provider}`} model={item.execution_model} models={models} working={working}
            save={(model) => change('execution_model', { execution_model: model })} />
        </Details> : null}
        {item.env.some((row) => row.required) ? <Variables item={item} required working={working} change={change} /> : null}
        {/* As on the web: connection, program and other variables are for whoever needs them; a problem in them opens the fold. */}
        <Details label="高级" open={Boolean(issue && issue.code !== 'env_missing')}>
          <View className="gap-4 pt-1">
            {/* Keyed by the roles, so switching one drops a test result that no longer matches what runs. */}
            {item.app_server ? <AppServerSettings key={item.roles.join()} server={item.app_server} working={working} save={change} test={test} /> : null}
            {manager ? <AssistantDetails key={`${manager.provider}:${manager.workspace}`} fields={['workspace']} manager={manager} models={models} working={working} save={saveAssistant} /> : null}
            <Field label="程序">
              <SaveRow value={bin} saved={item.bin} placeholder={item.bin.split(/[\\/]/).pop() ?? ''} mono working={working} onChange={setBin} onSave={() => void change('bin', { bin: bin.trim() })} />
            </Field>
            <Variables item={item} required={false} working={working} change={change} />
          </View>
        </Details>
        {issue?.code === 'launch_failed' ? <Details label="启动失败原因"><Text selectable className="font-mono text-meta">{issue.detail}</Text></Details> : null}
      </View>
    </> : null}
    {error ? <View className="px-4 pb-3"><ErrorText error={error} /></View> : null}
  </View>
}

const roles = {
  bot: { ...deviceCopy.roles.bot, icon: Bot },
  execution: { ...deviceCopy.roles.execution, icon: ListChecks },
}

function Toggle({ role, name, pressed, busy, disabled, onPress }: { role: keyof typeof roles; name: string; pressed: boolean; busy: boolean; disabled: boolean; onPress(): void }) {
  const { label, hint, icon } = roles[role]
  // Only one provider runs the bot, so that row draws a radio dot: tapping it moves the bot here, tapping it again turns it off, hence a toggle for assistive tech.
  // Executing tasks is a system switch any provider can have on. While a save runs the controls keep their look and ignore taps; the spinner says which one is changing.
  if (role === 'bot') return <Pressable accessibilityRole="togglebutton" accessibilityState={{ checked: pressed, disabled }} accessibilityLabel={`${name} ${label}`} accessibilityHint={hint(name)}
    onPress={disabled ? undefined : onPress} className="min-h-12 flex-row items-center gap-3 border-t border-border px-4 active:bg-muted">
    <Icon as={busy ? LoaderCircle : icon} className="text-muted-foreground" />
    <Text className="min-w-0 flex-1">{label}</Text>
    <View className={cn('size-5 items-center justify-center rounded-full border-2', pressed ? 'border-brand' : 'border-muted-foreground')}>
      {pressed ? <View className="size-2.5 rounded-full bg-brand" /> : null}
    </View>
  </Pressable>
  return <View className="min-h-12 flex-row items-center gap-3 border-t border-border px-4">
    <Icon as={busy ? LoaderCircle : icon} className="text-muted-foreground" />
    <Text className="min-w-0 flex-1">{label}</Text>
    <Switch value={pressed} accessibilityLabel={`${name} ${label}`} accessibilityHint={hint(name)} onValueChange={disabled ? undefined : onPress} />
  </View>
}

const modes: [AppServerMode, string, LucideIcon][] = [['system', deviceCopy.appServer.system, Monitor], ['spawn', deviceCopy.appServer.spawn, Play]]
const links: Record<AppServerLink['state'], [string, LucideIcon, string]> = {
  connected: ['text-success', CircleCheck, '已连接'],
  starting: ['text-muted-foreground', LoaderCircle, '连接中'],
  off: ['text-muted-foreground', CircleDashed, '未用于执行任务'],
  not_found: ['text-warning', CircleAlert, '未找到'],
  handshake_failed: ['text-warning', CircleAlert, '连不上'],
  launch_failed: ['text-warning', CircleAlert, '没能启动'],
}

/** Where Codex work runs: the App Server already running on that computer, or one the device starts. */
function AppServerSettings({ server, working, save, test }: { server: AppServer; working: boolean; save(key: string, change: Change): Promise<boolean>; test(): Promise<AppServerLink | null> }) {
  const [draft, setDraft] = useState({ mode: server.mode, socket: server.socket ?? '' })
  const [invalid, setInvalid] = useState(false)
  const [tested, setTested] = useState<AppServerLink | null>(null)
  const [testing, setTesting] = useState(false)
  useEffect(() => { setDraft({ mode: server.mode, socket: server.socket ?? '' }); setInvalid(false) }, [server.mode, server.socket])
  useEffect(() => setTested(null), [server.mode, server.socket])
  const dirty = draft.mode !== server.mode || draft.socket.trim() !== (server.socket ?? '')
  const saveConnection = () => {
    const socket = draft.socket.trim()
    if (draft.mode === 'system' && !socket.startsWith('/')) { setInvalid(true); return }
    setInvalid(false)
    void save('app_server', { app_server: { mode: draft.mode, socket: socket || null } })
  }
  const shown = tested ?? server.link
  const [tone, icon, label] = links[shown.state]
  return <Field label="连接方式">
    <View accessibilityRole="radiogroup" className="flex-row gap-1 self-start rounded-lg bg-muted p-1">
      {modes.map(([mode, text, modeIcon]) => <Button key={mode} accessibilityRole="radio" accessibilityState={{ checked: draft.mode === mode }} disabled={working}
        variant={draft.mode === mode ? 'outline' : 'ghost'} onPress={() => { setDraft({ ...draft, mode }); setInvalid(false) }}>
        <Icon as={modeIcon} /><Text>{text}</Text>
      </Button>)}
    </View>
    {draft.mode === 'system' ? <Input accessibilityLabel="App Server socket 的绝对路径" placeholder="App Server socket 的绝对路径" autoCapitalize="none" autoCorrect={false}
      className="font-mono" value={draft.socket} editable={!working} onChangeText={(socket) => { setDraft({ ...draft, socket }); setInvalid(false) }} /> : null}
    {draft.mode === 'system' ? <Text className="text-meta text-muted-foreground">共享所选外部 App Server 的会话，不隔离该服务的数据。</Text> : null}
    {invalid ? <Text role="alert" className="text-meta text-warning">填写运行 Codex 的电脑上的绝对路径。</Text> : null}
    <View className="flex-row flex-wrap items-center gap-2">
      {dirty ? <Button variant="outline" disabled={working} onPress={saveConnection}><Text>保存</Text></Button> : null}
      <Button variant="ghost" disabled={working || testing || dirty || server.link.state === 'off'} onPress={async () => { setTesting(true); try { setTested(await test()) } finally { setTesting(false) } }}>
        <Icon as={testing ? LoaderCircle : PlugZap} /><Text>测试连接</Text>
      </Button>
    </View>
    {dirty ? <Text className="text-meta text-muted-foreground">保存后可测试连接</Text> : <View className="flex-row items-center gap-2">
      <Icon as={icon} className={cn('size-4', tone)} />
      <Text className={cn('shrink text-meta', tone)}>{tested ? '测试：' : ''}{label}</Text>
    </View>}
    {!dirty && (shown.state === 'handshake_failed' || shown.state === 'launch_failed') ? <Text selectable className="font-mono text-meta text-muted-foreground">{shown.reason}</Text> : null}
  </Field>
}

const assistantFields = ['model', 'workspace'] as const

/**
 * The bot's model or folder, as `fields` picks, inside the settings of the provider it runs on. Keyed by its own saved fields and saving
 * only those over what is saved now, so saving one keeps the other's draft.
 */
function AssistantDetails({ fields, manager, models, working, save }: { fields: readonly (typeof assistantFields)[number][]; manager: ManagerSettings; models: ModelChoice[] | null; working: boolean; save(manager: ManagerSettings): Promise<boolean> }) {
  const [draft, setDraft] = useState(manager)
  const copy = deviceCopy.botModel
  // A model the list does not have stays typed in; 其他 opens the same field.
  const [chosen, setTyping] = useState<boolean | null>(null)
  const typing = chosen ?? modelTyped(manager.model, models)
  const cursor = manager.provider === 'cursor'
  const selected = models?.find((model) => draft.model.trim() ? model.id === draft.model.trim() : model.default)
  const reasoningEffort = selected?.effort.state === 'provider_controlled' ? null : draft.reasoning_effort
  const dirty = fields.some((key) => draft[key] !== manager[key]) || fields.includes('model') && (reasoningEffort !== manager.reasoning_effort || JSON.stringify(draft.backup) !== JSON.stringify(manager.backup))
  const efforts = selected?.effort.state === 'provider_controlled' ? [] : selected?.effort.state === 'selectable' ? selected.effort.options : effortOptions(manager.provider)
  const input = (key: (typeof assistantFields)[number], label: string, placeholder?: string) => <Input accessibilityLabel={label} className={key === 'model' ? 'min-w-0 flex-1' : 'font-mono'} autoCapitalize="none" autoCorrect={false} placeholder={placeholder}
    value={draft[key]} editable={!working} onChangeText={(text) => setDraft({ ...draft, [key]: text })} />
  return <View className="gap-3">
    {fields.includes('model') ? <><Field label={copy.label}>
      {models && !typing
        ? <ModelPicker models={models} value={draft.model} working={working} copy={copy} pick={(model) => setDraft({ ...draft, model })} other={() => { setTyping(true); setDraft({ ...draft, model: '' }) }} />
        : <View className="flex-row items-center gap-2">
          {input('model', copy.label, cursor ? 'auto' : copy.default)}
          {models ? <Button variant="ghost" size="icon" accessibilityLabel={copy.pick} disabled={working}
            onPress={() => { setTyping(false); setDraft({ ...draft, model: models.some((model) => model.id === draft.model) ? draft.model : '' }) }}><Icon as={List} /></Button> : null}
        </View>}
    </Field>
    <Field label="推理强度">
      {efforts.length ? <View className="flex-row flex-wrap gap-1">
        {[null, ...efforts].map((effort) => <Button key={effort ?? 'default'} variant={reasoningEffort === effort ? 'outline' : 'ghost'} disabled={working} onPress={() => setDraft({ ...draft, reasoning_effort: effort })}><Text>{effort ?? '由 AI 服务决定'}</Text></Button>)}
        <Text className="text-meta text-muted-foreground">用于下一回合，模型是否支持以 AI 服务的检查结果为准。</Text>
      </View> : <Text className="text-meta text-muted-foreground">由 AI 服务决定</Text>}
    </Field>
    <Details label="备用策略"><BackupPolicyEditor policy={draft.backup} disabled={working} change={(backup) => setDraft({ ...draft, backup })} /></Details>
    </> : null}
    {fields.includes('workspace') ? <Field label={folderLabels.botRun}>{input('workspace', folderLabels.botRun)}</Field> : null}
    {dirty ? <Button variant="outline" className="self-start" disabled={working || (cursor && !(fields.includes('model') ? draft.model : manager.model).trim()) || fields.includes('model') && draft.backup.candidates.some((candidate) => !candidate.model.trim())}
      onPress={() => void save({ ...manager, ...Object.fromEntries(fields.map((key) => [key, draft[key].trim()])),
        ...(fields.includes('model') ? { reasoning_effort: reasoningEffort, backup: { ...draft.backup, candidates: draft.backup.candidates.map((candidate) => ({ ...candidate, model: candidate.model.trim() })) } } : {}) })}><Text>保存</Text></Button> : null}
  </View>
}

function ExecutionDetails({ model, models, working, save }: { model: string; models: ModelChoice[] | null; working: boolean; save(model: string): Promise<boolean> }) {
  const [draft, setDraft] = useState(model)
  const [chosen, setTyping] = useState<boolean | null>(null)
  useEffect(() => { setDraft(model); setTyping(null) }, [model])
  const typing = chosen ?? modelTyped(model, models)
  const dirty = draft.trim() !== model
  return <View className="gap-2">
    <Field label={deviceCopy.executionModel.label}>
      {models && !typing ? <ModelPicker models={models} value={draft} working={working} copy={deviceCopy.executionModel} pick={setDraft} other={() => { setTyping(true); setDraft('') }} />
        : <View className="flex-row items-center gap-2">
          <Input accessibilityLabel={deviceCopy.executionModel.label} className="min-w-0 flex-1" autoCapitalize="none" autoCorrect={false} placeholder={deviceCopy.executionModel.default} value={draft} editable={!working} onChangeText={setDraft} />
          {models ? <Button variant="ghost" size="icon" accessibilityLabel={deviceCopy.executionModel.pick} disabled={working}
            onPress={() => { setTyping(false); setDraft(models.some((choice) => choice.id === draft) ? draft : '') }}><Icon as={List} /></Button> : null}
        </View>}
    </Field>
    {dirty ? <Button variant="outline" className="self-start" disabled={working} onPress={() => void save(draft.trim())}><Text>保存</Text></Button> : null}
  </View>
}

/** The web's model dropdown as a phone shows one: the choice in a row that opens the list in place. */
function ModelPicker({ models, value, working, copy = deviceCopy.botModel, pick, other }: { models: ModelChoice[]; value: string; working: boolean; copy?: typeof deviceCopy.botModel; pick(model: string): void; other(): void }) {
  const [open, setOpen] = useState(false)
  const options: [string, string][] = [['', copy.default], ...models.map((model): [string, string] => [model.id, model.name])]
  const shown = options.find(([id]) => id === value)?.[1] ?? value
  const choose = (next: () => void) => { setOpen(false); next() }
  return <View className="overflow-hidden rounded-lg border border-border">
    <Pressable accessibilityRole="button" accessibilityLabel={`${copy.label}，${shown}`} accessibilityState={{ expanded: open, disabled: working }} disabled={working}
      className="min-h-11 flex-row items-center gap-2 px-3 active:bg-muted" onPress={() => setOpen(!open)}>
      <Text numberOfLines={1} className="min-w-0 flex-1">{shown}</Text>
      <Icon as={open ? ChevronDown : ChevronRight} className="size-4 text-muted-foreground" />
    </Pressable>
    {open ? <View accessibilityRole="radiogroup">
      {options.map(([id, name]) => <Pressable key={id || 'default'} accessibilityRole="radio" accessibilityState={{ checked: id === value, disabled: working }} disabled={working}
        className="min-h-11 flex-row items-center gap-3 border-t border-border px-3 active:bg-muted" onPress={() => choose(() => pick(id))}>
        <Text numberOfLines={1} className="min-w-0 flex-1">{name}</Text>
        {id === value ? <Icon as={Check} className="size-4" /> : null}
      </Pressable>)}
      <Pressable accessibilityRole="button" accessibilityState={{ disabled: working }} disabled={working} className="min-h-11 flex-row items-center border-t border-border px-3 active:bg-muted" onPress={() => choose(other)}>
        <Text className="min-w-0 flex-1">{copy.other}</Text>
      </Pressable>
    </View> : null}
  </View>
}

/**
 * Each variable says only whether it is set; a value is typed once and never shown again. `required`: the keys the provider needs,
 * named after their service; otherwise the rest under their own names, with a field to add one.
 */
function Variables({ item, required, working, change }: { item: ProviderStatus; required: boolean; working: boolean; change(key: string, value: Change): Promise<boolean> }) {
  const [editing, setEditing] = useState<string | null>(null)
  const [value, setValue] = useState('')
  const [newName, setNewName] = useState('')
  const [newValue, setNewValue] = useState('')
  const addName = newName.trim().toUpperCase()
  const set = (name: string, next: string | null) => change(`env:${name}`, { env: { [name]: next } })
  const rows = item.env.filter((row) => row.required === required)
  const list = rows.map((row) => {
      const missing = item.state === 'needs_setup' && !row.set && !row.inherited && row.required
      return <View key={row.name} className="gap-2 rounded-lg bg-muted/60 py-1 ps-3 pe-1">
        <View className="min-h-11 flex-row items-center gap-2">
          <Text numberOfLines={1} className={cn('min-w-0 flex-1 text-meta', !required && 'font-mono')}>{required ? keyName(row.name) : row.name}</Text>
          <Text className={cn('text-meta', missing ? 'text-warning' : 'text-muted-foreground')}>{row.set ? '••••••' : row.inherited ? '沿用本机' : '未填'}</Text>
          <Button variant="ghost" size="icon" accessibilityLabel={`${row.set ? '替换' : '填写'} ${row.name}`} disabled={working} onPress={() => { setEditing(row.name); setValue('') }}><Icon as={Pencil} className="size-4" /></Button>
          {row.set ? <Button variant="ghost" size="icon" accessibilityLabel={`删除 ${row.name}`} disabled={working} onPress={() => void set(row.name, null)}><Icon as={Trash2} className="size-4" /></Button> : null}
        </View>
        {editing === row.name ? <View className="flex-row gap-2 pe-2 pb-1">
          <Input secureTextEntry autoFocus accessibilityLabel={`${row.name} 的值`} className="min-w-0 flex-1" maxLength={4096} value={value} onChangeText={setValue} />
          <Button variant="outline" disabled={working || !value} onPress={() => void set(row.name, value).then((ok) => { if (ok) { setEditing(null); setValue('') } })}><Text>保存</Text></Button>
          <Button variant="ghost" size="icon" accessibilityLabel="取消" onPress={() => { setEditing(null); setValue('') }}><Icon as={X} /></Button>
        </View> : null}
      </View>
    })
  if (required) return <View className="gap-2">{list}</View>
  return <Field label="环境变量">
    {list}
    <Input accessibilityLabel="变量名" placeholder="变量名" className="font-mono" autoCapitalize="characters" autoCorrect={false} maxLength={256} value={newName} editable={!working} onChangeText={setNewName} />
    <View className="flex-row gap-2">
      <Input secureTextEntry accessibilityLabel="值" placeholder="值" className="min-w-0 flex-1" maxLength={4096} value={newValue} editable={!working} onChangeText={setNewValue} />
      <Button variant="outline" disabled={working || !addName || !newValue} onPress={() => void set(addName, newValue).then((ok) => { if (ok) { setNewName(''); setNewValue('') } })}>
        <Icon as={Plus} /><Text>添加</Text>
      </Button>
    </View>
    <Text className="text-meta text-muted-foreground">只存在这台电脑上，保存后看不到原值</Text>
    {item.app_server?.mode === 'system' ? <Text className="text-meta text-muted-foreground">用已在运行的 Codex 时，这些变量不生效</Text> : null}
  </Field>
}

function SaveRow({ value, saved, placeholder, mono, working, onChange, onSave }: { value: string; saved: string; placeholder: string; mono?: boolean; working: boolean; onChange(value: string): void; onSave(): void }) {
  return <View className="flex-row gap-2">
    <Input accessibilityLabel={placeholder} placeholder={placeholder} autoCapitalize="none" autoCorrect={false} className={cn('min-w-0 flex-1', mono && 'font-mono')} value={value} editable={!working} onChangeText={onChange} />
    {value.trim() !== saved ? <Button variant="outline" disabled={working} onPress={onSave}><Text>保存</Text></Button> : null}
  </View>
}

function Section({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} />
      <Text role="heading" numberOfLines={1} className="shrink text-heading font-semibold">{title}</Text>
    </View>
    {children}
  </View>
}

function BackupPolicyEditor({ policy, disabled, change }: { policy: BackupPolicy; disabled: boolean; change(policy: BackupPolicy): void }) {
  const update = (index: number, value: BackupPolicy['candidates'][number]) => change({ ...policy, candidates: policy.candidates.map((candidate, at) => at === index ? value : candidate) })
  return <View className="gap-3">
    <View className="min-h-11 flex-row items-center gap-2"><Text className="min-w-0 flex-1 text-meta">自动尝试备用模型</Text><Switch accessibilityLabel="自动尝试备用模型" disabled={disabled || !policy.candidates.length} value={policy.automatic} onValueChange={(automatic) => change({ ...policy, automatic })} /></View>
    <Text className="text-meta text-muted-foreground">仅在原回合明确未执行时按顺序尝试一次。使用运行设备对应 AI 服务的登录账户，可能产生该账户的费用。</Text>
    {policy.candidates.map((candidate, index) => <View key={index} className="gap-2 rounded-lg border border-border p-2">
      <Text className="text-meta">备用模型 {index + 1}</Text>
      <View className="flex-row flex-wrap gap-1">{providerIds.map((provider) => <Button key={provider} variant={candidate.provider === provider ? 'outline' : 'ghost'} disabled={disabled} onPress={() => update(index, { provider, model: '', reasoning_effort: null })}><Text>{providerNames[provider]}</Text></Button>)}</View>
      <Input accessibilityLabel={`备用模型 ${index + 1}`} placeholder="模型名称" autoCapitalize="none" autoCorrect={false} value={candidate.model} editable={!disabled} onChangeText={(model) => update(index, { ...candidate, model })} />
      {effortOptions(candidate.provider).length ? <View className="flex-row flex-wrap gap-1">{[null, ...effortOptions(candidate.provider)].map((effort) => <Button key={effort ?? 'default'} variant={candidate.reasoning_effort === effort ? 'outline' : 'ghost'} disabled={disabled} onPress={() => update(index, { ...candidate, reasoning_effort: effort })}><Text>{effort ?? '由 AI 服务决定'}</Text></Button>)}</View> : <Text className="text-meta text-muted-foreground">推理强度由 AI 服务决定</Text>}
      <Button variant="ghost" disabled={disabled} onPress={() => { const candidates = policy.candidates.filter((_, at) => at !== index); change({ automatic: policy.automatic && candidates.length > 0, candidates }) }}><Text>移除备用模型</Text></Button>
    </View>)}
    <Button variant="outline" disabled={disabled || policy.candidates.length >= 5} onPress={() => change({ ...policy, candidates: [...policy.candidates, { provider: 'codex', model: '', reasoning_effort: null }] })}><Text>添加备用模型</Text></Button>
  </View>
}
